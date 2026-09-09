use axum::{
    body::Body,
    extract::{OriginalUri, Path, State},
    http::{header, StatusCode, HeaderMap},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::fs::{self, File};
use tokio::io::{self, AsyncSeekExt, AsyncReadExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;
use urlencoding::decode;
use crate::state::AppState;
use crate::cache::CachedStat;
use crate::error::S3ErrorType;
use futures_util::StreamExt;

/// Read-buffer size for streaming object bodies. `ReaderStream::new` defaults to 4 KiB
/// reads; 64 KiB cuts the per-chunk syscall/wakeup count 16x for large downloads.
const STREAM_BUF_SIZE: usize = 64 * 1024;

#[derive(Serialize)]
#[serde(rename = "CopyObjectResult")]
struct CopyObjectResult {
    #[serde(rename = "LastModified")]
    last_modified: String,
    #[serde(rename = "ETag")]
    etag: String,
}

#[derive(Serialize)]
#[serde(rename = "AccessControlPolicy")]
struct AccessControlPolicy {
    #[serde(rename = "Owner")]
    owner: AclOwner,
    #[serde(rename = "AccessControlList")]
    access_control_list: AccessControlList,
}

#[derive(Serialize)]
struct AclOwner {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "DisplayName")]
    display_name: String,
}

#[derive(Serialize)]
struct AccessControlList {
    #[serde(rename = "Grant")]
    grants: Vec<Grant>,
}

#[derive(Serialize)]
struct Grant {
    #[serde(rename = "Grantee")]
    grantee: Grantee,
    #[serde(rename = "Permission")]
    permission: String,
}

#[derive(Serialize)]
struct Grantee {
    #[serde(rename = "@xmlns:xsi")]
    xmlns_xsi: String,
    #[serde(rename = "@xsi:type")]
    xsi_type: String,
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "DisplayName")]
    display_name: String,
}

pub async fn get_object(
    Path((bucket, key)): Path<(String, String)>,
    uri: OriginalUri,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Response {
    let key = match decode(&key) {
        Ok(k) => k.into_owned(),
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let storage = match state.storage_map.get(&bucket) {
        Some(s) => s,
        None => return S3ErrorType::NoSuchBucket.to_response(Some(bucket)),
    };

    let path = match safe_join(storage, &key) {
        Some(p) => p,
        None => return S3ErrorType::AccessDenied.to_response(Some(key)),
    };
    let mut file = match File::open(&path).await {
        Ok(f) => f,
        Err(_) => return S3ErrorType::NoSuchKey.to_response(Some(key)),
    };

    let metadata = match file.metadata().await {
        Ok(m) => m,
        Err(_) => return S3ErrorType::InternalError.to_response(None),
    };

    let size = metadata.len();
    let mod_time: DateTime<Utc> = metadata.modified().unwrap_or(SystemTime::now()).into();
    let etag = format!("\"{:x}-{:x}\"", mod_time.timestamp_nanos_opt().unwrap_or(0), size);

    if has_acl_query(uri.0.query()) {
        return object_acl_response();
    }

    // Handle Range Header
    if let Some(range_header) = headers.get(header::RANGE).and_then(|h| h.to_str().ok()) {
        match parse_range(range_header, size) {
            RangeRequest::Satisfiable(start, end) => {
                let range_size = end - start + 1;
                if file.seek(io::SeekFrom::Start(start)).await.is_ok() {
                    let stream = ReaderStream::with_capacity(file.take(range_size), STREAM_BUF_SIZE);
                    return Response::builder()
                        .status(StatusCode::PARTIAL_CONTENT)
                        .header(header::CONTENT_TYPE, "application/octet-stream")
                        .header(header::CONTENT_LENGTH, range_size)
                        .header(header::CONTENT_RANGE, format!("bytes {}-{}/{}", start, end, size))
                        .header(header::ACCEPT_RANGES, "bytes")
                        .header(header::ETAG, etag)
                        .header(header::LAST_MODIFIED, http_date(mod_time))
                        .body(Body::from_stream(stream))
                        .unwrap();
                }
            }
            RangeRequest::Unsatisfiable => {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{}", size))
                    .body(Body::empty())
                    .unwrap();
            }
            RangeRequest::Invalid => {
                // Fallthrough to standard 200 OK
            }
        }
    }

    let stream = ReaderStream::with_capacity(file, STREAM_BUF_SIZE);
    let body = Body::from_stream(stream);

    Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, size)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, etag)
        .header(header::LAST_MODIFIED, http_date(mod_time))
        .body(body)
        .unwrap()
}

pub async fn head_object(
    Path((bucket, key)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
) -> Response {
    let key = match decode(&key) {
        Ok(k) => k.into_owned(),
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let cache_key = format!("{}/{}", bucket, key);
    if let Some(cached) = state.cache.get(&cache_key) {
        return Response::builder()
            .header(header::CONTENT_LENGTH, cached.size)
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::ETAG, &cached.etag)
            .header(header::LAST_MODIFIED, http_date(cached.mod_time))
            .body(Body::empty())
            .unwrap();
    }

    let storage = match state.storage_map.get(&bucket) {
        Some(s) => s,
        None => return S3ErrorType::NoSuchBucket.to_response(Some(bucket)),
    };

    let path = match safe_join(storage, &key) {
        Some(p) => p,
        None => return S3ErrorType::AccessDenied.to_response(Some(key)),
    };
    let metadata = match fs::metadata(&path).await {
        Ok(m) => m,
        Err(_) => return S3ErrorType::NoSuchKey.to_response(Some(key)),
    };

    if metadata.is_dir() {
         return S3ErrorType::NoSuchKey.to_response(Some(key));
    }

    let size = metadata.len();
    let mod_time: DateTime<Utc> = metadata.modified().unwrap_or(SystemTime::now()).into();
    let etag = format!("\"{:x}-{:x}\"", mod_time.timestamp_nanos_opt().unwrap_or(0), size);

    state.cache.insert(cache_key, CachedStat {
        size,
        mod_time,
        etag: etag.clone(),
    });

    Response::builder()
        .header(header::CONTENT_LENGTH, size)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, etag)
        .header(header::LAST_MODIFIED, http_date(mod_time))
        .body(Body::empty())
        .unwrap()
}

pub async fn put_object(
    Path((bucket, key)): Path<(String, String)>,
    uri: OriginalUri,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let key = match decode(&key) {
        Ok(k) => k.into_owned(),
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let storage = match state.storage_map.get(&bucket) {
        Some(s) => s,
        None => return S3ErrorType::NoSuchBucket.to_response(Some(bucket)),
    };

    // A PUT carrying the `?acl` subresource is PutObjectAcl — an ACL write, not an
    // object write. We don't persist ACLs, but we must not fall through to the
    // object-write path below, which would `File::create` (truncate) the object and
    // overwrite its body with the ACL payload. Acknowledge it as a no-op.
    if has_acl_query(uri.0.query()) {
        return StatusCode::OK.into_response();
    }

    let path = match safe_join(storage, &key) {
        Some(p) => p,
        None => return S3ErrorType::AccessDenied.to_response(Some(key)),
    };

    if let Some(copy_source) = headers
        .get("x-amz-copy-source")
        .and_then(|value| value.to_str().ok())
    {
        return copy_object(&state, &bucket, &key, &path, copy_source).await;
    }

    // Create parent directories
    if !ensure_parent_dir(&path).await {
        return S3ErrorType::InternalError.to_response(None);
    }

    // Write to a temporary file in the destination directory, then atomically rename
    // it over the target. The live object stays intact until the upload fully
    // succeeds, so an aborted or failed PUT never truncates or partially overwrites
    // existing data, and concurrent PUTs to the same key can't interleave.
    let temp_path = temp_path_for(&path);
    let mut file = match create_retrying_on_pruned_dir(&path, &temp_path).await {
        Ok(f) => f,
        Err(_) => return S3ErrorType::InternalError.to_response(None),
    };

    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(data) => {
                if file.write_all(&data).await.is_err() {
                    let _ = fs::remove_file(&temp_path).await;
                    return S3ErrorType::InternalError.to_response(None);
                }
            }
            Err(_) => {
                let _ = fs::remove_file(&temp_path).await;
                return S3ErrorType::InternalError.to_response(None);
            }
        }
    }

    // Flush before we claim success: a write error surfacing only on close (ENOSPC/EIO)
    // must fail the request, not be swallowed when the file is dropped. The fsync is
    // configurable — it dominates small-PUT latency, and deployments where the proxy is
    // not the source of truth may prefer to skip it.
    if file.flush().await.is_err() {
        let _ = fs::remove_file(&temp_path).await;
        return S3ErrorType::InternalError.to_response(None);
    }
    if state.config.fsync && file.sync_all().await.is_err() {
        let _ = fs::remove_file(&temp_path).await;
        return S3ErrorType::InternalError.to_response(None);
    }
    drop(file);

    if fs::rename(&temp_path, &path).await.is_err() {
        let _ = fs::remove_file(&temp_path).await;
        return S3ErrorType::InternalError.to_response(None);
    }

    // Invalidate the stat cache only after the new object is in place.
    state.cache.remove(&format!("{}/{}", bucket, key));

    // Return the ETag the S3 PutObject API is expected to provide, and prime the stat
    // cache with the fresh metadata so the common upload-then-HEAD pattern hits it.
    let etag = match fs::metadata(&path).await {
        Ok(metadata) => {
            let mod_time: DateTime<Utc> = metadata.modified().unwrap_or(SystemTime::now()).into();
            let etag = format!(
                "\"{:x}-{:x}\"",
                mod_time.timestamp_nanos_opt().unwrap_or(0),
                metadata.len()
            );
            state.cache.insert(
                format!("{}/{}", bucket, key),
                CachedStat { size: metadata.len(), mod_time, etag: etag.clone() },
            );
            etag
        }
        Err(_) => return S3ErrorType::InternalError.to_response(None),
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::ETAG, etag)
        .body(Body::empty())
        .unwrap()
}

/// Create `path`'s parent directory. Returns false only if it could not be created.
async fn ensure_parent_dir(path: &std::path::Path) -> bool {
    match path.parent() {
        Some(parent) => fs::create_dir_all(parent).await.is_ok(),
        None => true,
    }
}

/// Create `temp_path`, recreating the destination directory and retrying once.
///
/// Empty-directory pruning is what makes that retry necessary: a DELETE removing the last
/// object of a directory can prune it between this write's `create_dir_all` and the create
/// below, and the upload would fail with a spurious 500. The window is small but real —
/// `aws s3 sync --delete` writes and deletes in the same tree concurrently.
async fn create_retrying_on_pruned_dir(
    path: &std::path::Path,
    temp_path: &std::path::Path,
) -> io::Result<File> {
    match File::create(temp_path).await {
        Ok(file) => Ok(file),
        Err(err) => {
            if !ensure_parent_dir(path).await {
                return Err(err);
            }
            File::create(temp_path).await
        }
    }
}

/// Build a unique temporary path in the same directory as `path`, so a subsequent
/// rename onto `path` is atomic (same filesystem). Hidden (dot-prefixed) and suffixed
/// with pid + a monotonic counter to avoid collisions between concurrent uploads.
fn temp_path_for(path: &std::path::Path) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("object");
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".{}.{}.{}.tmp", name, std::process::id(), n))
}

async fn copy_object(
    state: &Arc<AppState>,
    destination_bucket: &str,
    destination_key: &str,
    destination_path: &std::path::Path,
    copy_source: &str,
) -> Response {
    let (source_bucket, source_key) = match parse_copy_source(copy_source) {
        Some(source) => source,
        None => return StatusCode::BAD_REQUEST.into_response(),
    };

    let source_storage = match state.storage_map.get(&source_bucket) {
        Some(storage) => storage,
        None => return S3ErrorType::NoSuchBucket.to_response(Some(source_bucket)),
    };

    let source_path = match safe_join(source_storage, &source_key) {
        Some(p) => p,
        None => return S3ErrorType::AccessDenied.to_response(Some(source_key)),
    };
    match fs::metadata(&source_path).await {
        Ok(metadata) if !metadata.is_dir() => {}
        Ok(_) => return S3ErrorType::NoSuchKey.to_response(Some(source_key)),
        Err(_) => return S3ErrorType::NoSuchKey.to_response(Some(source_key)),
    };

    // Reject a copy of an object onto itself. `fs::copy` opens the destination with
    // O_TRUNC before reading the source, so when both paths resolve to the same inode
    // the object is truncated to 0 bytes. S3 rejects self-copies (unless metadata is
    // being changed, which we don't support) rather than performing them.
    if let (Ok(src_canon), Ok(dst_canon)) = (
        fs::canonicalize(&source_path).await,
        fs::canonicalize(destination_path).await,
    ) {
        if src_canon == dst_canon {
            return S3ErrorType::InvalidRequest.to_response(Some(destination_key.to_string()));
        }
    }

    if !ensure_parent_dir(destination_path).await {
        return S3ErrorType::InternalError.to_response(None);
    }

    if fs::copy(&source_path, destination_path).await.is_err() {
        // Same pruned-directory race as PUT: recreate the destination directory and try
        // once more before calling it a server error.
        if !ensure_parent_dir(destination_path).await
            || fs::copy(&source_path, destination_path).await.is_err()
        {
            return S3ErrorType::InternalError.to_response(None);
        }
    }

    state
        .cache
        .remove(&format!("{}/{}", destination_bucket, destination_key));

    // Build the result from the destination's own metadata so the returned ETag/
    // LastModified match a subsequent HEAD/GET of the copied object (the source's
    // pre-copy mtime would not).
    let dest_metadata = match fs::metadata(destination_path).await {
        Ok(metadata) => metadata,
        Err(_) => return S3ErrorType::InternalError.to_response(None),
    };
    let mod_time: DateTime<Utc> = dest_metadata
        .modified()
        .unwrap_or(SystemTime::now())
        .into();
    let etag = format!(
        "\"{:x}-{:x}\"",
        mod_time.timestamp_nanos_opt().unwrap_or(0),
        dest_metadata.len()
    );
    let result = CopyObjectResult {
        last_modified: mod_time.to_rfc3339(),
        etag,
    };

    let xml = quick_xml::se::to_string(&result).unwrap_or_default();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/xml")
        .body(Body::from(xml))
        .unwrap()
}

pub async fn delete_object(
    Path((bucket, key)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
) -> Response {
    let key = match decode(&key) {
        Ok(k) => k.into_owned(),
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let storage = match state.storage_map.get(&bucket) {
        Some(s) => s,
        None => return S3ErrorType::NoSuchBucket.to_response(Some(bucket)),
    };

    let path = match safe_join(storage, &key) {
        Some(p) => p,
        None => return S3ErrorType::AccessDenied.to_response(Some(key)),
    };
    // S3 returns 204 even if the file doesn't exist during DELETE, so a failure here is
    // not an error — but it does change where the prune below starts.
    let removed = fs::remove_file(&path).await.is_ok();
    if removed {
        state.cache.remove(&format!("{}/{}", bucket, key));
    }

    if state.config.prune_empty_dirs {
        // On the normal path the object is gone and its directory may now be empty, so
        // start at the parent. When `remove_file` failed the key may have named a
        // directory (`DELETE /bucket/logs/`), so start at the path itself and let an
        // emptied directory be collected too. `remove_dir` refuses a non-empty directory
        // either way, so neither start can destroy anything.
        let start = if removed { path.parent() } else { Some(path.as_path()) };
        if let Some(start) = start {
            prune_empty_dirs(storage, start).await;
        }
    }

    StatusCode::NO_CONTENT.into_response()
}

/// Remove `from` and each empty directory above it, stopping at `root` — the bucket's
/// storage directory, which is never removed however empty the bucket becomes.
///
/// S3 has no directories: a client "deletes a folder" by deleting every key under it, and
/// without this the emptied directory tree stays on disk forever. Listing hides those
/// directories (`subtree_has_file`), so they are invisible over the API and accumulate
/// unnoticed on the storage mount.
///
/// This cannot delete data. `remove_dir` — never `remove_dir_all` — succeeds only on an
/// empty directory, so the first directory still holding anything ends the walk: a
/// sibling object, a subdirectory, or the temp file of a PUT in flight. Errors are
/// ignored for the same reason the delete's own error is: the object is gone, and a
/// leftover directory is not worth failing a 204 over.
async fn prune_empty_dirs(root: &std::path::Path, from: &std::path::Path) {
    let mut dir = from;
    // `safe_join` guarantees `from` sits under `root`; `starts_with` keeps that true for
    // every step, so a surprising path can't walk the parent chain out of the bucket.
    while dir != root && dir.starts_with(root) {
        if fs::remove_dir(dir).await.is_err() {
            return;
        }
        dir = match dir.parent() {
            Some(parent) => parent,
            None => return,
        };
    }
}

enum RangeRequest {
    Satisfiable(u64, u64),
    Unsatisfiable,
    Invalid,
}

fn parse_range(range_header: &str, file_size: u64) -> RangeRequest {
    if !range_header.starts_with("bytes=") { return RangeRequest::Invalid; }
    let range_str = &range_header[6..];
    let parts: Vec<&str> = range_str.split('-').collect();
    if parts.len() != 2 { return RangeRequest::Invalid; }

    let start_str = parts[0].trim();
    let end_str = parts[1].trim();

    if start_str.is_empty() && end_str.is_empty() {
        return RangeRequest::Invalid;
    }

    if start_str.is_empty() {
        // Suffix range
        let Ok(suffix_len) = end_str.parse::<u64>() else {
            return RangeRequest::Invalid;
        };
        if suffix_len == 0 {
            return RangeRequest::Invalid;
        }
        if file_size == 0 {
            return RangeRequest::Unsatisfiable;
        }
        let start = file_size.saturating_sub(suffix_len);
        let end = file_size - 1;
        return RangeRequest::Satisfiable(start, end);
    }

    let Ok(start) = start_str.parse::<u64>() else {
        return RangeRequest::Invalid;
    };

    if start >= file_size {
        return RangeRequest::Unsatisfiable;
    }

    let end = if end_str.is_empty() {
        file_size.saturating_sub(1)
    } else {
        let Ok(parsed_end) = end_str.parse::<u64>() else {
            return RangeRequest::Invalid;
        };
        std::cmp::min(parsed_end, file_size.saturating_sub(1))
    };

    if start <= end {
        RangeRequest::Satisfiable(start, end)
    } else {
        RangeRequest::Invalid
    }
}

/// Format a UTC timestamp as an HTTP-date (RFC 7231 IMF-fixdate), e.g.
/// "Sun, 06 Nov 1994 08:49:37 GMT". `DateTime::to_rfc2822` emits a "+0000" offset
/// instead of "GMT", which is not a valid HTTP-date and is rejected by strict clients.
fn http_date(dt: DateTime<Utc>) -> String {
    dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn has_acl_query(query: Option<&str>) -> bool {
    query
        .map(|value| value.split('&').any(|part| part == "acl" || part.starts_with("acl=")))
        .unwrap_or(false)
}

fn object_acl_response() -> Response {
    let response = AccessControlPolicy {
        owner: AclOwner {
            id: owner_id(),
            display_name: "Owner".to_string(),
        },
        access_control_list: AccessControlList {
            grants: vec![Grant {
                grantee: Grantee {
                    xmlns_xsi: "http://www.w3.org/2001/XMLSchema-instance".to_string(),
                    xsi_type: "CanonicalUser".to_string(),
                    id: owner_id(),
                    display_name: "Owner".to_string(),
                },
                permission: "FULL_CONTROL".to_string(),
            }],
        },
    };

    let xml = quick_xml::se::to_string(&response).unwrap_or_default();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/xml")
        .body(Body::from(xml))
        .unwrap()
}

fn parse_copy_source(copy_source: &str) -> Option<(String, String)> {
    let decoded = urlencoding::decode(copy_source).ok()?.into_owned();
    let trimmed = decoded.trim_start_matches('/');
    let (bucket, key) = trimmed.split_once('/')?;
    Some((bucket.to_string(), key.to_string()))
}

fn owner_id() -> String {
    "75aa57f09aa0c8caeab4f8c24e99d10f8e7faeebf76c078efc7c6caea54ba06a".to_string()
}

fn safe_join(storage: &std::path::Path, key: &str) -> Option<std::path::PathBuf> {
    use std::path::Component;
    let mut resolved = storage.to_path_buf();
    for component in std::path::Path::new(key).components() {
        match component {
            Component::Normal(c) => resolved.push(c),
            Component::RootDir | Component::CurDir => continue,
            Component::Prefix(_) | Component::ParentDir => return None,
        }
    }
    Some(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_parse_range() {
        // Normal ranges
        assert!(matches!(parse_range("bytes=0-499", 1000), RangeRequest::Satisfiable(0, 499)));
        assert!(matches!(parse_range("bytes=500-", 1000), RangeRequest::Satisfiable(500, 999)));
        
        // Suffix ranges
        assert!(matches!(parse_range("bytes=-500", 1000), RangeRequest::Satisfiable(500, 999)));
        assert!(matches!(parse_range("bytes=-1500", 1000), RangeRequest::Satisfiable(0, 999)));

        // Unsatisfiable
        assert!(matches!(parse_range("bytes=1000-", 1000), RangeRequest::Unsatisfiable));
        assert!(matches!(parse_range("bytes=9999-", 1000), RangeRequest::Unsatisfiable));
        
        // Zero length files
        assert!(matches!(parse_range("bytes=-500", 0), RangeRequest::Unsatisfiable));
        assert!(matches!(parse_range("bytes=0-", 0), RangeRequest::Unsatisfiable));

        // Invalid ranges
        assert!(matches!(parse_range("bytes=abc-def", 1000), RangeRequest::Invalid));
        assert!(matches!(parse_range("bytes=-", 1000), RangeRequest::Invalid));
        assert!(matches!(parse_range("bytes=500-499", 1000), RangeRequest::Invalid));
        assert!(matches!(parse_range("wrong=0-100", 1000), RangeRequest::Invalid));
    }

    #[test]
    fn test_safe_join() {
        let storage = Path::new("/var/data");

        // Normal keys
        assert_eq!(safe_join(storage, "my_file.txt").unwrap(), Path::new("/var/data/my_file.txt"));
        assert_eq!(safe_join(storage, "folder/file.txt").unwrap(), Path::new("/var/data/folder/file.txt"));

        // Leading slashes are ignored (RootDir)
        assert_eq!(safe_join(storage, "/folder/file.txt").unwrap(), Path::new("/var/data/folder/file.txt"));

        // Current dir dots are ignored
        assert_eq!(safe_join(storage, "./folder/./file.txt").unwrap(), Path::new("/var/data/folder/file.txt"));

        // ParentDir traversal is rejected
        assert!(safe_join(storage, "../etc/passwd").is_none());
        assert!(safe_join(storage, "folder/../../etc/passwd").is_none());

        // Windows drive prefixes are only parsed as `Prefix` components on Windows,
        // where they are rejected. On Unix a string like "C:/..." is just a normal
        // (contained) key, so it resolves safely inside storage instead.
        #[cfg(windows)]
        assert!(safe_join(storage, "C:/Windows/System32").is_none());
        #[cfg(not(windows))]
        assert_eq!(
            safe_join(storage, "C:/Windows/System32").unwrap(),
            Path::new("/var/data/C:/Windows/System32")
        );
    }

    #[tokio::test]
    async fn prune_walks_up_the_whole_emptied_chain() {
        // The shape a client leaves behind after deleting the last key under a prefix.
        let root = tempfile::TempDir::new().unwrap();
        let deepest = root.path().join("a/b/c");
        std::fs::create_dir_all(&deepest).unwrap();

        prune_empty_dirs(root.path(), &deepest).await;

        assert!(!root.path().join("a").exists(), "the emptied tree should be gone");
        assert!(root.path().exists(), "the bucket's storage directory must survive");
    }

    #[tokio::test]
    async fn prune_stops_at_a_directory_that_still_holds_something() {
        // `a/` keeps a sibling object, so only the empty `a/b` may go.
        let root = tempfile::TempDir::new().unwrap();
        let empty = root.path().join("a/b");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(root.path().join("a/keep.txt"), b"data").unwrap();

        prune_empty_dirs(root.path(), &empty).await;

        assert!(!empty.exists(), "the empty directory should be pruned");
        assert!(root.path().join("a/keep.txt").exists(), "a sibling object must survive");
    }

    #[tokio::test]
    async fn prune_never_removes_the_bucket_root() {
        // Deleting the last object in a bucket empties the storage directory itself; it
        // is configuration, not data, and must stay.
        let root = tempfile::TempDir::new().unwrap();

        prune_empty_dirs(root.path(), root.path()).await;

        assert!(root.path().exists());
    }
}

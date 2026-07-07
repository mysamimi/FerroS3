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
        if let Some(range) = parse_range(range_header, size) {
            let (start, end) = range;
            let range_size = end - start + 1;
            
            if file.seek(io::SeekFrom::Start(start)).await.is_ok() {
                let stream = ReaderStream::new(file.take(range_size));
                return Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header(header::CONTENT_LENGTH, range_size)
                    .header(header::CONTENT_RANGE, format!("bytes {}-{}/{}", start, end, size))
                    .header(header::ETAG, etag)
                    .header("Last-Modified", mod_time.to_rfc2822())
                    .body(Body::from_stream(stream))
                    .unwrap();
            }
        }
    }

    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, size)
        .header(header::ETAG, etag)
        .header("Last-Modified", mod_time.to_rfc2822())
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
            .header(header::ETAG, &cached.etag)
            .header("Last-Modified", cached.mod_time.to_rfc2822())
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
        .header(header::ETAG, etag)
        .header("Last-Modified", mod_time.to_rfc2822())
        .body(Body::empty())
        .unwrap()
}

pub async fn put_object(
    Path((bucket, key)): Path<(String, String)>,
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
    if let Some(parent) = path.parent() {
        if let Err(_) = fs::create_dir_all(parent).await {
            return S3ErrorType::InternalError.to_response(None);
        }
    }

    let mut file = match File::create(&path).await {
        Ok(f) => f,
        Err(_) => return S3ErrorType::InternalError.to_response(None),
    };

    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(data) => {
                if let Err(_) = file.write_all(&data).await {
                    return S3ErrorType::InternalError.to_response(None);
                }
            }
            Err(_) => return S3ErrorType::InternalError.to_response(None),
        }
    }

    // Invalidate cache
    state.cache.remove(&format!("{}/{}", bucket, key));

    StatusCode::OK.into_response()
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
    let source_metadata = match fs::metadata(&source_path).await {
        Ok(metadata) if !metadata.is_dir() => metadata,
        Ok(_) => return S3ErrorType::NoSuchKey.to_response(Some(source_key)),
        Err(_) => return S3ErrorType::NoSuchKey.to_response(Some(source_key)),
    };

    if let Some(parent) = destination_path.parent() {
        if let Err(_) = fs::create_dir_all(parent).await {
            return S3ErrorType::InternalError.to_response(None);
        }
    }

    if let Err(_) = fs::copy(&source_path, destination_path).await {
        return S3ErrorType::InternalError.to_response(None);
    }

    state
        .cache
        .remove(&format!("{}/{}", destination_bucket, destination_key));

    let mod_time: DateTime<Utc> = source_metadata
        .modified()
        .unwrap_or(SystemTime::now())
        .into();
    let etag = format!(
        "\"{:x}-{:x}\"",
        mod_time.timestamp_nanos_opt().unwrap_or(0),
        source_metadata.len()
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
    if let Err(_) = fs::remove_file(&path).await {
        // S3 returns 204 even if file doesn't exist during DELETE
        return StatusCode::NO_CONTENT.into_response();
    }

    state.cache.remove(&format!("{}/{}", bucket, key));
    StatusCode::NO_CONTENT.into_response()
}

fn parse_range(range_header: &str, file_size: u64) -> Option<(u64, u64)> {
    if !range_header.starts_with("bytes=") { return None; }
    let range_str = &range_header[6..];
    let parts: Vec<&str> = range_str.split('-').collect();
    if parts.len() != 2 { return None; }

    let start = parts[0].parse::<u64>().ok()?;
    let end = if parts[1].is_empty() {
        file_size - 1
    } else {
        parts[1].parse::<u64>().ok()?
    };

    if start <= end && end < file_size {
        Some((start, end))
    } else {
        None
    }
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
}

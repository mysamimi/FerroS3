use axum::{
    body::Body,
    extract::{Path, State, Query},
    http::{header},
    response::Response,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::SystemTime;
use crate::state::AppState;
use crate::error::S3ErrorType;
use utoipa::{IntoParams, ToSchema};

#[derive(Deserialize, IntoParams, ToSchema)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "kebab-case")]
pub struct ListObjectsParams {
    /// Only return object keys that start with this prefix.
    pub prefix: Option<String>,
    /// Use a delimiter to group common prefixes.
    pub delimiter: Option<String>,
    /// Compatibility marker for legacy list requests.
    pub marker: Option<String>,
    /// Maximum number of keys to return.
    pub max_keys: Option<usize>,
    /// Use `2` to request the simplified ListObjectsV2-compatible mode.
    #[serde(rename = "list-type")]
    pub list_type: Option<u8>,
    /// Compatibility token accepted by the simplified V2 mode.
    pub continuation_token: Option<String>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename = "ListBucketResult")]
pub struct ListBucketResult {
    #[serde(rename = "@xmlns")]
    pub xmlns: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Prefix")]
    pub prefix: String,
    #[serde(rename = "Marker", skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
    #[serde(rename = "NextMarker", skip_serializing_if = "Option::is_none")]
    pub next_marker: Option<String>,
    #[serde(rename = "MaxKeys")]
    pub max_keys: usize,
    #[serde(rename = "Delimiter", skip_serializing_if = "Option::is_none")]
    pub delimiter: Option<String>,
    #[serde(rename = "IsTruncated")]
    pub is_truncated: bool,
    #[serde(rename = "Contents")]
    pub contents: Vec<ObjectContent>,
    #[serde(rename = "CommonPrefixes", skip_serializing_if = "Vec::is_empty")]
    pub common_prefixes: Vec<CommonPrefix>,
    // V2 fields
    #[serde(rename = "KeyCount", skip_serializing_if = "Option::is_none")]
    pub key_count: Option<usize>,
    #[serde(rename = "ContinuationToken", skip_serializing_if = "Option::is_none")]
    pub continuation_token: Option<String>,
    #[serde(rename = "NextContinuationToken", skip_serializing_if = "Option::is_none")]
    pub next_continuation_token: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct ObjectContent {
    #[serde(rename = "Key")]
    pub key: String,
    #[serde(rename = "LastModified")]
    pub last_modified: String,
    #[serde(rename = "ETag")]
    pub etag: String,
    #[serde(rename = "Size")]
    pub size: u64,
    #[serde(rename = "StorageClass")]
    pub storage_class: String,
}

#[derive(Serialize, ToSchema)]
pub struct CommonPrefix {
    #[serde(rename = "Prefix")]
    pub prefix: String,
}

pub async fn list_objects(
    Path(bucket): Path<String>,
    Query(params): Query<ListObjectsParams>,
    State(state): State<Arc<AppState>>,
) -> Response {
    let storage = match state.storage_map.get(&bucket) {
        Some(s) => s,
        None => return S3ErrorType::NoSuchBucket.to_response(Some(bucket)),
    };

    let prefix = params.prefix.unwrap_or_default();
    let delimiter = params.delimiter;
    let max_keys = params.max_keys.unwrap_or(1000);

    // Exclusive start key: ListObjects v1 uses `marker`, v2 uses `continuation-token`.
    let start_after = params.continuation_token.clone().or_else(|| params.marker.clone());

    // Walk the bucket on a blocking thread: `walkdir` and the per-entry `stat` calls are
    // synchronous, so running them directly on an async worker would block the runtime for
    // the entire traversal. The walk is scoped to the subtree implied by `prefix`, emits
    // items already in key order, and stops one item past the page — so `max-keys=1` on a
    // million-key prefix costs one page, not a full traversal.
    let storage = storage.clone();
    let walk_prefix = prefix.clone();
    let walk_delimiter = delimiter.clone();
    let walk_start = start_after.clone();
    // One item beyond the page: its existence is exactly what makes the page truncated.
    let scan_limit = max_keys.saturating_add(1);
    let items = match tokio::task::spawn_blocking(move || {
        collect_entries(
            &storage,
            &walk_prefix,
            walk_delimiter.as_deref(),
            walk_start.as_deref(),
            scan_limit,
        )
    })
    .await
    {
        Ok(collected) => collected,
        Err(_) => return S3ErrorType::InternalError.to_response(None),
    };

    // Emit up to max_keys in order; if another item remains, mark truncated and record
    // the last emitted key as the next page's start token.
    let mut contents = Vec::new();
    let mut common_prefixes_out = Vec::new();
    let mut is_truncated = false;
    let mut next_key: Option<String> = None;
    for (k, item) in items {
        if contents.len() + common_prefixes_out.len() >= max_keys {
            is_truncated = true;
            break;
        }
        next_key = Some(k);
        match item {
            ListItem::Content(c) => contents.push(*c),
            ListItem::Prefix(p) => common_prefixes_out.push(CommonPrefix { prefix: p }),
        }
    }

    let key_count = contents.len() + common_prefixes_out.len();
    let is_v2 = params.list_type == Some(2);
    let next_token = if is_truncated { next_key } else { None };

    let result = ListBucketResult {
        xmlns: "http://s3.amazonaws.com/doc/2006-03-01/".to_string(),
        name: bucket,
        prefix,
        marker: params.marker,
        next_marker: if is_v2 { None } else { next_token.clone() },
        max_keys,
        delimiter,
        is_truncated,
        contents,
        common_prefixes: common_prefixes_out,
        key_count: if is_v2 { Some(key_count) } else { None },
        continuation_token: params.continuation_token,
        next_continuation_token: if is_v2 { next_token } else { None },
    };

    let xml = quick_xml::se::to_string(&result).unwrap();
    Response::builder()
        .header(header::CONTENT_TYPE, "application/xml")
        .body(Body::from(xml))
        .unwrap()
}

/// One entry of a listing page: either an object or a delimiter-collapsed prefix. Both
/// are ordered by the same key, so a page can be built from a single ordered walk.
enum ListItem {
    Content(Box<ObjectContent>),
    Prefix(String),
}

/// Walk `storage` (scoped to the prefix's subtree) and collect at most `limit` listing
/// items, in ascending key order, skipping anything at or before `start_after`.
///
/// The traversal is an explicit depth-first stack rather than `walkdir` because every
/// saving here comes from *not* opening a directory: `walkdir` reads and sorts a
/// directory's entries at the moment it hands you the directory, so a decision made
/// after that has already paid for the readdir. Owning the stack means each directory is
/// tested — against the prefix, against `start_after`, against a delimiter collapse —
/// before it is ever read.
///
/// Entries are sorted by the name a *key* carries, so a directory sorts as `name/`. That
/// is what makes depth-first order equal key order: `a.txt` precedes everything under
/// `a/` because '.' (0x2E) < '/' (0x2F), and `ab.txt` follows it. Since the walk never
/// produces a key smaller than one it already produced, it can stop the moment `limit`
/// items are collected instead of enumerating the whole subtree and sorting afterwards.
///
/// Blocking: readdir and the per-object `stat` are synchronous, so this runs on a
/// `spawn_blocking` thread.
fn collect_entries(
    storage: &std::path::Path,
    prefix: &str,
    delimiter: Option<&str>,
    start_after: Option<&str>,
    limit: usize,
) -> Vec<(String, ListItem)> {
    let mut items: Vec<(String, ListItem)> = Vec::new();
    if limit == 0 {
        return items;
    }

    // Root the walk at the deepest directory the prefix guarantees. An escaping prefix
    // matches no valid key (keys never contain `..`), so return nothing.
    let search_root = match prefix_search_root(storage, prefix) {
        Some(root) => root,
        None => return items,
    };
    // Keys are relative to `storage`, so the walk root contributes the leading part of
    // every key it yields. Deriving it from the path (not from `prefix`) keeps the keys
    // normalised, whatever spelling the prefix used.
    let root_key = match search_root.strip_prefix(storage) {
        Ok(rel) => {
            let key = rel_path_to_key(rel);
            if key.is_empty() {
                String::new()
            } else {
                format!("{}/", key)
            }
        }
        Err(_) => return items,
    };

    // Delimiter-collapsed prefixes are contiguous in key order (every member shares the
    // string prefix), but the set also guards against a filesystem that hands back an
    // unexpected order: a group is emitted once, at its first member's position.
    let mut seen_prefixes: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    // The group whose fate is already decided — emitted as a CommonPrefix, or dropped
    // because it sits at or before `start_after`. Every remaining key under it collapses
    // to the same prefix and so adds nothing to the page. Groups are contiguous in key
    // order, so one slot is enough.
    let mut settled_group: Option<String> = None;

    let mut stack: Vec<std::vec::IntoIter<Child>> = vec![read_children(&search_root, &root_key)];

    while !stack.is_empty() {
        let child = match stack.last_mut().and_then(|frame| frame.next()) {
            Some(child) => child,
            None => {
                stack.pop();
                continue;
            }
        };

        if child.is_dir {
            // `child.key` is the subtree: the directory's key with its trailing '/'.
            if settled_group.as_deref().is_some_and(|g| child.key.starts_with(g)) {
                continue;
            }
            if !subtree_may_contain(&child.key, prefix, start_after) {
                continue;
            }

            // Whole-directory collapse: with '/' as the delimiter, every key under this
            // directory carries the same CommonPrefix, so the page needs one fact about
            // it — that it holds a key at all. An unsorted probe that stops at the first
            // file settles that without reading and sorting a directory of a hundred
            // thousand names. Requiring a real key is also what keeps an empty directory
            // out of the listing.
            if let Some(group) = collapsing_group(&child.key, prefix, delimiter) {
                let after_start = start_after.is_none_or(|s| group > s);
                if after_start && !seen_prefixes.contains(group) && subtree_has_file(&child.path) {
                    seen_prefixes.insert(group.to_string());
                    items.push((group.to_string(), ListItem::Prefix(group.to_string())));
                    if items.len() >= limit {
                        break;
                    }
                }
                continue;
            }

            stack.push(read_children(&child.path, &child.key));
            continue;
        }

        let key = child.key;

        // A key inside a settled group: the group already stands (or was ruled out).
        if settled_group.as_deref().is_some_and(|g| key.starts_with(g)) {
            continue;
        }
        if !key.starts_with(prefix) {
            continue;
        }

        // Delimiter grouping for a delimiter that falls inside a name rather than on a
        // directory boundary ("a-1.txt" collapsing at "-"); '/' groups are already
        // collapsed above. The CommonPrefix stands at its first member's position.
        if let Some(d) = delimiter {
            let relative_to_prefix = &key[prefix.len()..];
            if let Some(idx) = relative_to_prefix.find(d) {
                let common_prefix = format!("{}{}{}", prefix, &relative_to_prefix[..idx], d);
                let after_start = start_after.is_none_or(|s| common_prefix.as_str() > s);
                let is_new = seen_prefixes.insert(common_prefix.clone());

                // Abandon the rest of this directory only when the group boundary is at
                // or above it. A delimiter inside a name leaves the directory holding
                // keys of other groups, which must still be visited.
                let parent = match key.rfind('/') {
                    Some(i) => &key[..=i],
                    None => "",
                };
                if parent.starts_with(common_prefix.as_str()) {
                    stack.pop();
                }
                settled_group = Some(common_prefix.clone());

                if after_start && is_new {
                    items.push((common_prefix.clone(), ListItem::Prefix(common_prefix)));
                    if items.len() >= limit {
                        break;
                    }
                }
                continue;
            }
        }

        // Exclusive start key (marker / continuation-token). Filtering before the stat
        // below means an earlier page's keys cost a readdir entry, not a syscall each.
        if start_after.is_some_and(|s| key.as_str() <= s) {
            continue;
        }

        // A file that vanished between readdir and stat (e.g. a concurrent DELETE):
        // skip it rather than panicking on unwrap. `symlink_metadata` keeps a symlink
        // reported as itself, as the walk has always done.
        let metadata = match std::fs::symlink_metadata(&child.path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mod_time: DateTime<Utc> = metadata.modified().unwrap_or(SystemTime::now()).into();
        let etag = format!("\"{:x}-{:x}\"", mod_time.timestamp_nanos_opt().unwrap_or(0), metadata.len());

        items.push((
            key.clone(),
            ListItem::Content(Box::new(ObjectContent {
                key,
                last_modified: mod_time.to_rfc3339(),
                etag,
                size: metadata.len(),
                storage_class: "STANDARD".to_string(),
            })),
        ));
        if items.len() >= limit {
            break;
        }
    }

    items
}

/// One entry of a directory, carrying the key it contributes: a file's own key, or a
/// directory's subtree (its key plus '/'). Building the key here — once, from the parent's
/// key — also means no path is re-derived per entry.
struct Child {
    key: String,
    path: std::path::PathBuf,
    is_dir: bool,
}

/// Read one directory into key order. The handle is closed before returning, so the walk
/// holds one open directory at a time however deep it goes. An unreadable directory (a
/// permission-denied subtree) yields nothing rather than ending the listing.
fn read_children(dir: &std::path::Path, key_prefix: &str) -> std::vec::IntoIter<Child> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new().into_iter(),
    };
    let mut children: Vec<Child> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            // A symlink is not followed, so it counts as an object, not a directory.
            let is_dir = entry.file_type().ok()?.is_dir();
            let name = entry.file_name().to_string_lossy().into_owned();
            let key = if is_dir {
                format!("{}{}/", key_prefix, name)
            } else {
                format!("{}{}", key_prefix, name)
            };
            Some(Child { key, path: entry.path(), is_dir })
        })
        .collect();
    // Sorting by the key (a directory's ends in '/') is what makes depth-first order
    // equal ascending key order.
    children.sort_by(|a, b| a.key.cmp(&b.key));
    children.into_iter()
}

/// The single CommonPrefix a directory collapses into, if it does. With '/' as the
/// delimiter every key under `subtree` (the directory key plus '/') shares one prefix,
/// namely `subtree` itself — provided the subtree lies inside `prefix` and the delimiter
/// falls at this directory's own boundary rather than deeper in.
fn collapsing_group<'a>(subtree: &'a str, prefix: &str, delimiter: Option<&str>) -> Option<&'a str> {
    if delimiter != Some("/") {
        return None;
    }
    let remainder = subtree.strip_prefix(prefix)?;
    // The directory that *is* the prefix collapses nothing; its children do.
    let inner = remainder.strip_suffix('/')?;
    if inner.is_empty() || inner.contains('/') {
        return None;
    }
    Some(subtree)
}

/// Whether `dir` holds at least one object anywhere beneath it. Unsorted and lazy: it
/// stops at the first file, so proving a collapsed group non-empty costs a readdir or
/// two rather than a full ordered traversal.
fn subtree_has_file(dir: &std::path::Path) -> bool {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .any(|entry| !entry.file_type().is_dir())
}

/// Whether a directory can still hold a key this page wants. `subtree` is its key plus
/// '/', which every key under it starts with: the directory is worth reading only when
/// that subtree overlaps the prefix, and only when it isn't entirely at or before
/// `start_after`.
fn subtree_may_contain(subtree: &str, prefix: &str, start_after: Option<&str>) -> bool {
    // Either the subtree lies inside the prefix, or the prefix reaches into the subtree.
    if !subtree.starts_with(prefix) && !prefix.starts_with(subtree) {
        return false;
    }
    // `start_after` past the whole subtree: every key in it starts with `subtree`, and a
    // start key greater than `subtree` that doesn't extend it is greater than all of them.
    match start_after {
        Some(s) => s < subtree || s.starts_with(subtree),
        None => true,
    }
}

/// Derive the deepest directory a prefixed walk can start from. The prefix is split at its
/// last '/': the leading path becomes a subdirectory of `storage` that roots the walk,
/// while the trailing segment stays a filename filter applied to keys (so `logs/2024`
/// still matches both `logs/2024.txt` and `logs/2024/jan.txt`). Returns `None` when the
/// prefix path would escape `storage`.
fn prefix_search_root(storage: &std::path::Path, prefix: &str) -> Option<std::path::PathBuf> {
    let dir_part = match prefix.rfind('/') {
        Some(idx) => &prefix[..idx],
        None => return Some(storage.to_path_buf()),
    };
    let mut root = storage.to_path_buf();
    for component in std::path::Path::new(dir_part).components() {
        match component {
            std::path::Component::Normal(c) => root.push(c),
            std::path::Component::RootDir | std::path::Component::CurDir => continue,
            std::path::Component::Prefix(_) | std::path::Component::ParentDir => return None,
        }
    }
    Some(root)
}

/// Turn a storage-relative path into an S3 key by joining its `Normal` components with
/// '/'. On Unix this preserves backslashes that are legal in filenames; on Windows it
/// normalises the native '\' separators to '/'.
fn rel_path_to_key(rel: &std::path::Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::collections::HashMap;
    use quick_cache::sync::Cache;
    use tokio::fs;
    use axum::body::to_bytes;
    use crate::config::{Config, BucketConfig};

    async fn setup_test_state(bucket_name: &str, storage_path: &str) -> Arc<AppState> {
        let mut storage_map = HashMap::new();
        storage_map.insert(bucket_name.to_string(), PathBuf::from(storage_path));
        
        let config = Config {
            port: 8080,
            endpoint: "0.0.0.0".to_string(),
            verbose: false,
            cache_size: 10,
            fsync: true,
            request_timeout_secs: 30,
            auth: None,
            buckets: vec![BucketConfig { name: bucket_name.to_string(), storage: storage_path.to_string() }],
        };

        Arc::new(AppState {
            config,
            cache: Cache::new(10),
            storage_map,
        })
    }

    async fn create_test_files(base: &str, files: &[&str]) {
        for f in files {
            let path = PathBuf::from(base).join(f);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).await.unwrap();
            }
            fs::write(&path, "data").await.unwrap();
        }
    }

    #[tokio::test]
    async fn test_list_objects_truncation_and_keycount() {
        let storage = "./test_list_data";
        let bucket = "test_bucket";
        let _ = fs::remove_dir_all(storage).await;
        fs::create_dir_all(storage).await.unwrap();

        create_test_files(storage, &["a.txt", "b.txt", "c.txt"]).await;

        let state = setup_test_state(bucket, storage).await;

        // Test max_keys = 2 (Should be truncated)
        let params = ListObjectsParams {
            prefix: None,
            delimiter: None,
            marker: None,
            max_keys: Some(2),
            list_type: Some(2),
            continuation_token: None,
        };
        let response = list_objects(Path(bucket.to_string()), Query(params), State(state.clone())).await;
        let (_, body) = response.into_parts();
        let xml = String::from_utf8(to_bytes(body, usize::MAX).await.unwrap().to_vec()).unwrap();

        assert!(xml.contains("<IsTruncated>true</IsTruncated>"));
        assert!(xml.contains("<KeyCount>2</KeyCount>"));

        // Test max_keys = 3 (Exactly matching total files, Should NOT be truncated)
        let params_exact = ListObjectsParams {
            prefix: None,
            delimiter: None,
            marker: None,
            max_keys: Some(3),
            list_type: Some(2),
            continuation_token: None,
        };
        let response_exact = list_objects(Path(bucket.to_string()), Query(params_exact), State(state.clone())).await;
        let (_, body_exact) = response_exact.into_parts();
        let xml_exact = String::from_utf8(to_bytes(body_exact, usize::MAX).await.unwrap().to_vec()).unwrap();

        assert!(xml_exact.contains("<IsTruncated>false</IsTruncated>"));
        assert!(xml_exact.contains("<KeyCount>3</KeyCount>"));

        let _ = fs::remove_dir_all(storage).await;
    }

    #[tokio::test]
    async fn test_list_objects_delimiter() {
        let storage = "./test_list_data_delim";
        let bucket = "test_bucket";
        let _ = fs::remove_dir_all(storage).await;
        fs::create_dir_all(storage).await.unwrap();

        create_test_files(storage, &["folder1/a.txt", "folder1/b.txt", "folder2/c.txt", "root.txt"]).await;

        let state = setup_test_state(bucket, storage).await;

        let params = ListObjectsParams {
            prefix: None,
            delimiter: Some("/".to_string()),
            marker: None,
            max_keys: Some(10),
            list_type: Some(2),
            continuation_token: None,
        };
        
        let response = list_objects(Path(bucket.to_string()), Query(params), State(state.clone())).await;
        let (_, body) = response.into_parts();
        let xml = String::from_utf8(to_bytes(body, usize::MAX).await.unwrap().to_vec()).unwrap();

        assert!(xml.contains("<Key>root.txt</Key>"));
        assert!(xml.contains("<Prefix>folder1/</Prefix>"));
        assert!(xml.contains("<Prefix>folder2/</Prefix>"));
        assert!(xml.contains("<KeyCount>3</KeyCount>"));
        assert!(xml.contains("<IsTruncated>false</IsTruncated>"));

        let _ = fs::remove_dir_all(storage).await;
    }

    async fn list_xml(state: &Arc<AppState>, bucket: &str, params: ListObjectsParams) -> String {
        let response = list_objects(Path(bucket.to_string()), Query(params), State(state.clone())).await;
        let (_, body) = response.into_parts();
        String::from_utf8(to_bytes(body, usize::MAX).await.unwrap().to_vec()).unwrap()
    }

    fn v2(max_keys: usize, continuation_token: Option<String>) -> ListObjectsParams {
        ListObjectsParams {
            prefix: None,
            delimiter: None,
            marker: None,
            max_keys: Some(max_keys),
            list_type: Some(2),
            continuation_token,
        }
    }

    #[tokio::test]
    async fn test_list_objects_returns_keys_in_ascending_order() {
        let storage = "./test_list_data_sorted";
        let bucket = "test_bucket";
        let _ = fs::remove_dir_all(storage).await;
        fs::create_dir_all(storage).await.unwrap();
        // Created out of order; the listing must still be sorted.
        create_test_files(storage, &["c.txt", "a.txt", "b.txt"]).await;
        let state = setup_test_state(bucket, storage).await;

        let xml = list_xml(&state, bucket, v2(1000, None)).await;
        let a = xml.find("<Key>a.txt</Key>").unwrap();
        let b = xml.find("<Key>b.txt</Key>").unwrap();
        let c = xml.find("<Key>c.txt</Key>").unwrap();
        assert!(a < b && b < c, "keys not in ascending order: {xml}");

        let _ = fs::remove_dir_all(storage).await;
    }

    #[tokio::test]
    async fn test_list_objects_marker_skips_prior_keys() {
        let storage = "./test_list_data_marker";
        let bucket = "test_bucket";
        let _ = fs::remove_dir_all(storage).await;
        fs::create_dir_all(storage).await.unwrap();
        create_test_files(storage, &["a.txt", "b.txt", "c.txt"]).await;
        let state = setup_test_state(bucket, storage).await;

        let params = ListObjectsParams {
            prefix: None,
            delimiter: None,
            marker: Some("a.txt".to_string()),
            max_keys: Some(1000),
            list_type: None,
            continuation_token: None,
        };
        let xml = list_xml(&state, bucket, params).await;
        assert!(!xml.contains("<Key>a.txt</Key>"), "marker should exclude a.txt: {xml}");
        assert!(xml.contains("<Key>b.txt</Key>"));
        assert!(xml.contains("<Key>c.txt</Key>"));

        let _ = fs::remove_dir_all(storage).await;
    }

    #[tokio::test]
    async fn test_list_objects_prefix_returns_matching_subtree() {
        let storage = "./test_list_data_prefix";
        let bucket = "test_bucket";
        let _ = fs::remove_dir_all(storage).await;
        fs::create_dir_all(storage).await.unwrap();
        create_test_files(
            storage,
            &[
                "photos/2024/a.jpg",
                "photos/2024/b.jpg",
                "photos/2023/c.jpg",
                "docs/readme.txt",
            ],
        )
        .await;
        let state = setup_test_state(bucket, storage).await;

        // Prefix ending in '/': walk is rooted at photos/2024, only those keys returned.
        let params = ListObjectsParams {
            prefix: Some("photos/2024/".to_string()),
            delimiter: None,
            marker: None,
            max_keys: Some(1000),
            list_type: Some(2),
            continuation_token: None,
        };
        let xml = list_xml(&state, bucket, params).await;
        assert!(xml.contains("<Key>photos/2024/a.jpg</Key>"), "{xml}");
        assert!(xml.contains("<Key>photos/2024/b.jpg</Key>"), "{xml}");
        assert!(!xml.contains("photos/2023"), "{xml}");
        assert!(!xml.contains("docs/readme.txt"), "{xml}");
        assert!(xml.contains("<KeyCount>2</KeyCount>"), "{xml}");

        let _ = fs::remove_dir_all(storage).await;
    }

    #[tokio::test]
    async fn test_list_objects_prefix_without_trailing_slash_spans_boundary() {
        // A prefix whose last segment is a partial name must still match both a file and a
        // directory sharing that partial name, since the walk roots at the parent dir.
        let storage = "./test_list_data_prefix_partial";
        let bucket = "test_bucket";
        let _ = fs::remove_dir_all(storage).await;
        fs::create_dir_all(storage).await.unwrap();
        create_test_files(
            storage,
            &["logs/2024.txt", "logs/2024/jan.txt", "logs/2025.txt"],
        )
        .await;
        let state = setup_test_state(bucket, storage).await;

        let params = ListObjectsParams {
            prefix: Some("logs/2024".to_string()),
            delimiter: None,
            marker: None,
            max_keys: Some(1000),
            list_type: Some(2),
            continuation_token: None,
        };
        let xml = list_xml(&state, bucket, params).await;
        assert!(xml.contains("<Key>logs/2024.txt</Key>"), "{xml}");
        assert!(xml.contains("<Key>logs/2024/jan.txt</Key>"), "{xml}");
        assert!(!xml.contains("logs/2025.txt"), "{xml}");

        let _ = fs::remove_dir_all(storage).await;
    }

    #[test]
    fn subtree_may_contain_prunes_unreachable_directories() {
        // Inside the prefix: read it.
        assert!(subtree_may_contain("photos/2024/", "photos/", None));
        // The prefix reaches into this directory: read it.
        assert!(subtree_may_contain("photos/", "photos/2024/", None));
        // Neither: the subtree cannot hold a matching key.
        assert!(!subtree_may_contain("docs/", "photos/", None));
        // Entirely before the start key: every key under `a/` is below `b`.
        assert!(!subtree_may_contain("a/", "", Some("b")));
        // The start key points into this subtree, so part of it is still wanted.
        assert!(subtree_may_contain("a/", "", Some("a/m.txt")));
    }

    #[test]
    fn collapsing_group_only_fires_on_a_directory_boundary() {
        // A child directory of the prefix collapses into exactly its own subtree.
        assert_eq!(collapsing_group("photos/2024/", "photos/", Some("/")), Some("photos/2024/"));
        // A partial prefix still collapses at the directory it names.
        assert_eq!(collapsing_group("photos/2024/", "photos/20", Some("/")), Some("photos/2024/"));
        // The prefix's own directory collapses nothing — its children are the groups.
        assert_eq!(collapsing_group("photos/", "photos/", Some("/")), None);
        // A directory the prefix reaches into is not a group.
        assert_eq!(collapsing_group("photos/", "photos/2024/", Some("/")), None);
        // Only '/' collapses whole directories; another delimiter cuts inside names.
        assert_eq!(collapsing_group("photos/2024/", "photos/", Some("-")), None);
        assert_eq!(collapsing_group("photos/2024/", "photos/", None), None);
    }

    #[tokio::test]
    async fn test_list_objects_orders_keys_across_directory_boundaries() {
        // '.' (0x2E) sorts before '/' (0x2F), so `a.txt` precedes everything under `a/`,
        // and `ab.txt` follows it. Depth-first order must reproduce that key order.
        let storage = "./test_list_data_boundary";
        let bucket = "test_bucket";
        let _ = fs::remove_dir_all(storage).await;
        fs::create_dir_all(storage).await.unwrap();
        create_test_files(storage, &["ab.txt", "a/x.txt", "a.txt"]).await;
        let state = setup_test_state(bucket, storage).await;

        let xml = list_xml(&state, bucket, v2(1000, None)).await;
        let dot = xml.find("<Key>a.txt</Key>").unwrap();
        let nested = xml.find("<Key>a/x.txt</Key>").unwrap();
        let sibling = xml.find("<Key>ab.txt</Key>").unwrap();
        assert!(dot < nested && nested < sibling, "keys not in ascending order: {xml}");

        // The same order must survive one-key pages, which is what the early-exit walk
        // relies on: each page resumes exactly where the previous one stopped.
        let mut token: Option<String> = None;
        let mut seen = Vec::new();
        for _ in 0..3 {
            let xml = list_xml(&state, bucket, v2(1, token.clone())).await;
            let start = xml.find("<Key>").unwrap() + "<Key>".len();
            let end = xml[start..].find("</Key>").unwrap() + start;
            let key = xml[start..end].to_string();
            seen.push(key.clone());
            token = Some(key);
        }
        assert_eq!(seen, vec!["a.txt", "a/x.txt", "ab.txt"]);

        let _ = fs::remove_dir_all(storage).await;
    }

    #[tokio::test]
    async fn test_list_objects_paginates_into_a_subdirectory() {
        // A continuation token pointing inside a subtree must resume within it, even
        // though whole directories below the start key are pruned from the walk.
        let storage = "./test_list_data_deep_page";
        let bucket = "test_bucket";
        let _ = fs::remove_dir_all(storage).await;
        fs::create_dir_all(storage).await.unwrap();
        create_test_files(storage, &["a/1.txt", "a/2.txt", "a/3.txt", "b/1.txt"]).await;
        let state = setup_test_state(bucket, storage).await;

        let xml = list_xml(&state, bucket, v2(2, Some("a/1.txt".to_string()))).await;
        assert!(!xml.contains("<Key>a/1.txt</Key>"), "{xml}");
        assert!(xml.contains("<Key>a/2.txt</Key>"), "{xml}");
        assert!(xml.contains("<Key>a/3.txt</Key>"), "{xml}");
        assert!(xml.contains("<IsTruncated>true</IsTruncated>"), "{xml}");
        assert!(xml.contains("<NextContinuationToken>a/3.txt</NextContinuationToken>"), "{xml}");

        // Last page: only b/1.txt is left.
        let xml = list_xml(&state, bucket, v2(2, Some("a/3.txt".to_string()))).await;
        assert!(xml.contains("<Key>b/1.txt</Key>"), "{xml}");
        assert!(xml.contains("<KeyCount>1</KeyCount>"), "{xml}");
        assert!(xml.contains("<IsTruncated>false</IsTruncated>"), "{xml}");

        let _ = fs::remove_dir_all(storage).await;
    }

    #[tokio::test]
    async fn test_list_objects_stats_only_the_page_it_returns() {
        // The walk must stop one item past the page: with max-keys=1 over 50 objects,
        // exactly one object is returned and the page is marked truncated.
        let storage = "./test_list_data_early_exit";
        let bucket = "test_bucket";
        let _ = fs::remove_dir_all(storage).await;
        fs::create_dir_all(storage).await.unwrap();
        let names: Vec<String> = (0..50).map(|i| format!("k{i:03}.txt")).collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        create_test_files(storage, &refs).await;
        let state = setup_test_state(bucket, storage).await;

        let xml = list_xml(&state, bucket, v2(1, None)).await;
        assert_eq!(xml.matches("<Key>").count(), 1, "{xml}");
        assert!(xml.contains("<Key>k000.txt</Key>"), "{xml}");
        assert!(xml.contains("<IsTruncated>true</IsTruncated>"), "{xml}");

        let _ = fs::remove_dir_all(storage).await;
    }

    fn v2_delim(delimiter: &str, prefix: Option<&str>, max_keys: usize) -> ListObjectsParams {
        ListObjectsParams {
            prefix: prefix.map(|p| p.to_string()),
            delimiter: Some(delimiter.to_string()),
            marker: None,
            max_keys: Some(max_keys),
            list_type: Some(2),
            continuation_token: None,
        }
    }

    #[tokio::test]
    async fn test_list_objects_delimiter_ignores_empty_directories() {
        // A CommonPrefix is still derived from a real key, so a directory holding no
        // objects — including one that only holds other empty directories — reports
        // nothing, exactly as it did before the walk learned to prune settled groups.
        let storage = "./test_list_data_empty_dirs";
        let bucket = "test_bucket";
        let _ = fs::remove_dir_all(storage).await;
        fs::create_dir_all(format!("{storage}/empty")).await.unwrap();
        fs::create_dir_all(format!("{storage}/hollow/inner")).await.unwrap();
        create_test_files(storage, &["full/a.txt"]).await;
        let state = setup_test_state(bucket, storage).await;

        let xml = list_xml(&state, bucket, v2_delim("/", None, 1000)).await;
        assert!(xml.contains("<Prefix>full/</Prefix>"), "{xml}");
        assert!(!xml.contains("empty/"), "{xml}");
        assert!(!xml.contains("hollow/"), "{xml}");
        assert!(xml.contains("<KeyCount>1</KeyCount>"), "{xml}");

        let _ = fs::remove_dir_all(storage).await;
    }

    #[tokio::test]
    async fn test_list_objects_delimiter_collapses_deep_subtrees() {
        // Every key under `f1/` collapses to one CommonPrefix, however deep it sits, and
        // the sibling directory must still be reported.
        let storage = "./test_list_data_deep_group";
        let bucket = "test_bucket";
        let _ = fs::remove_dir_all(storage).await;
        fs::create_dir_all(storage).await.unwrap();
        create_test_files(
            storage,
            &["f1/sub/deep/x.txt", "f1/sub/zzz.txt", "f1/other/y.txt", "f2/b.txt", "top.txt"],
        )
        .await;
        let state = setup_test_state(bucket, storage).await;

        let xml = list_xml(&state, bucket, v2_delim("/", None, 1000)).await;
        assert!(xml.contains("<Prefix>f1/</Prefix>"), "{xml}");
        assert!(xml.contains("<Prefix>f2/</Prefix>"), "{xml}");
        assert!(xml.contains("<Key>top.txt</Key>"), "{xml}");
        assert!(!xml.contains("deep"), "collapsed subtree leaked into the page: {xml}");
        assert!(xml.contains("<KeyCount>3</KeyCount>"), "{xml}");

        let _ = fs::remove_dir_all(storage).await;
    }

    #[tokio::test]
    async fn test_list_objects_delimiter_inside_a_filename_keeps_siblings() {
        // The delimiter need not be '/'. Here the group boundary falls inside a file
        // name, so the directory also holds keys outside the group: pruning must not
        // swallow them.
        let storage = "./test_list_data_delim_infix";
        let bucket = "test_bucket";
        let _ = fs::remove_dir_all(storage).await;
        fs::create_dir_all(storage).await.unwrap();
        create_test_files(storage, &["d/dxa.txt", "d/dxb.txt", "d/dzz.dat"]).await;
        let state = setup_test_state(bucket, storage).await;

        let xml = list_xml(&state, bucket, v2_delim("x", Some("d/"), 1000)).await;
        assert!(xml.contains("<Prefix>d/dx</Prefix>"), "{xml}");
        assert!(xml.contains("<Key>d/dzz.dat</Key>"), "{xml}");
        assert!(xml.contains("<KeyCount>2</KeyCount>"), "{xml}");

        let _ = fs::remove_dir_all(storage).await;
    }

    #[tokio::test]
    async fn test_list_objects_delimiter_pagination_skips_settled_groups() {
        // Page 2 must resume after the first group without re-listing its members.
        let storage = "./test_list_data_delim_page";
        let bucket = "test_bucket";
        let _ = fs::remove_dir_all(storage).await;
        fs::create_dir_all(storage).await.unwrap();
        create_test_files(storage, &["f1/a.txt", "f1/b.txt", "f2/c.txt", "f3/d.txt"]).await;
        let state = setup_test_state(bucket, storage).await;

        let mut params = v2_delim("/", None, 1);
        let xml = list_xml(&state, bucket, params).await;
        assert!(xml.contains("<Prefix>f1/</Prefix>"), "{xml}");
        assert!(xml.contains("<IsTruncated>true</IsTruncated>"), "{xml}");
        assert!(xml.contains("<NextContinuationToken>f1/</NextContinuationToken>"), "{xml}");

        params = v2_delim("/", None, 2);
        params.continuation_token = Some("f1/".to_string());
        let xml = list_xml(&state, bucket, params).await;
        assert!(!xml.contains("<Prefix>f1/</Prefix>"), "{xml}");
        assert!(xml.contains("<Prefix>f2/</Prefix>"), "{xml}");
        assert!(xml.contains("<Prefix>f3/</Prefix>"), "{xml}");
        assert!(xml.contains("<KeyCount>2</KeyCount>"), "{xml}");
        assert!(xml.contains("<IsTruncated>false</IsTruncated>"), "{xml}");

        let _ = fs::remove_dir_all(storage).await;
    }

    /// Every object key under `dir`, found by a plain recursive walk: the reference the
    /// real listing is checked against.
    fn reference_keys(dir: &std::path::Path, base: &str) -> Vec<String> {
        let mut keys = Vec::new();
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return keys,
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().unwrap().is_dir() {
                keys.extend(reference_keys(&entry.path(), &format!("{base}{name}/")));
            } else {
                keys.push(format!("{base}{name}"));
            }
        }
        keys
    }

    /// The page S3 defines for these parameters, computed the slow, obvious way: take
    /// every key, apply prefix, delimiter and start key, sort, then cut.
    fn reference_page(
        keys: &[String],
        prefix: &str,
        delimiter: Option<&str>,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Vec<String> {
        let mut items: Vec<String> = Vec::new();
        for key in keys.iter().filter(|k| k.starts_with(prefix)) {
            let item = match delimiter.and_then(|d| key[prefix.len()..].find(d).map(|i| (d, i))) {
                Some((d, i)) => format!("{}{}{}", prefix, &key[prefix.len()..][..i], d),
                None => key.clone(),
            };
            if start_after.is_some_and(|s| item.as_str() <= s) {
                continue;
            }
            if !items.contains(&item) {
                items.push(item);
            }
        }
        items.sort();
        items.truncate(max_keys);
        items
    }

    /// The keys and prefixes a listing actually returned, in the order the pages emit
    /// them: one item per page, so the sequence is the walk's own ordering.
    async fn paged_items(
        state: &Arc<AppState>,
        bucket: &str,
        prefix: Option<&str>,
        delimiter: Option<&str>,
        pages: usize,
    ) -> Vec<String> {
        let mut token: Option<String> = None;
        let mut seen = Vec::new();
        for _ in 0..pages {
            let params = ListObjectsParams {
                prefix: prefix.map(|p| p.to_string()),
                delimiter: delimiter.map(|d| d.to_string()),
                marker: None,
                max_keys: Some(1),
                list_type: Some(2),
                continuation_token: token.clone(),
            };
            let xml = list_xml(state, bucket, params).await;
            // Only the item elements count: the response also echoes the request's own
            // <Prefix>, which is not part of the page.
            let item = ["<Key>", "<CommonPrefixes><Prefix>"].iter().find_map(|tag| {
                let open = xml.find(tag)? + tag.len();
                let close = xml[open..].find('<')? + open;
                Some(xml[open..close].to_string())
            });
            match item {
                Some(item) => {
                    token = Some(item.clone());
                    seen.push(item);
                }
                None => break,
            }
        }
        seen
    }

    #[tokio::test]
    async fn test_list_objects_matches_a_reference_listing() {
        // A tree with the orderings that trip naive walks: a file and a directory
        // sharing a name stem, names around '.' and '-' (both below '/'), nesting of
        // uneven depth, and an empty directory that must never surface.
        let storage = "./test_list_data_reference";
        let bucket = "test_bucket";
        let _ = fs::remove_dir_all(storage).await;
        fs::create_dir_all(format!("{storage}/vacant")).await.unwrap();
        create_test_files(
            storage,
            &[
                "a.txt",
                "a/1.txt",
                "a/2/deep.txt",
                "a/2/deeper/x.txt",
                "a-b.txt",
                "ab/c.txt",
                "b.txt",
                "b/1.txt",
                "b/2.txt",
                "media/2024/01/clip.ts",
                "media/2024/02/clip.ts",
                "media/2023/clip.ts",
                "zz.txt",
            ],
        )
        .await;
        let state = setup_test_state(bucket, storage).await;
        let keys = reference_keys(std::path::Path::new(storage), "");

        // Flat listing, one key per page: the sequence must equal sorted key order.
        let expected = reference_page(&keys, "", None, None, usize::MAX);
        assert_eq!(paged_items(&state, bucket, None, None, 30).await, expected);

        // Delimiter listing: directories collapse, `vacant/` reports nothing.
        let expected = reference_page(&keys, "", Some("/"), None, usize::MAX);
        assert_eq!(paged_items(&state, bucket, None, Some("/"), 30).await, expected);
        assert!(!expected.iter().any(|item| item == "vacant/"), "{expected:?}");

        // Prefixed listing, with and without a delimiter.
        let expected = reference_page(&keys, "media/", None, None, usize::MAX);
        assert_eq!(paged_items(&state, bucket, Some("media/"), None, 30).await, expected);
        let expected = reference_page(&keys, "media/", Some("/"), None, usize::MAX);
        assert_eq!(paged_items(&state, bucket, Some("media/"), Some("/"), 30).await, expected);

        // A prefix that ends mid-name, so the walk starts above the matching entries.
        let expected = reference_page(&keys, "a", None, None, usize::MAX);
        assert_eq!(paged_items(&state, bucket, Some("a"), None, 30).await, expected);

        let _ = fs::remove_dir_all(storage).await;
    }

    #[tokio::test]
    async fn test_list_objects_pagination_roundtrip() {
        let storage = "./test_list_data_page";
        let bucket = "test_bucket";
        let _ = fs::remove_dir_all(storage).await;
        fs::create_dir_all(storage).await.unwrap();
        create_test_files(storage, &["a.txt", "b.txt", "c.txt"]).await;
        let state = setup_test_state(bucket, storage).await;

        // Page 1: one key, truncated, next token points past a.txt.
        let x1 = list_xml(&state, bucket, v2(1, None)).await;
        assert!(x1.contains("<Key>a.txt</Key>"));
        assert!(!x1.contains("<Key>b.txt</Key>"));
        assert!(x1.contains("<IsTruncated>true</IsTruncated>"));
        assert!(x1.contains("<NextContinuationToken>a.txt</NextContinuationToken>"));

        // Page 2: continue from a.txt → b.txt, still truncated.
        let x2 = list_xml(&state, bucket, v2(1, Some("a.txt".to_string()))).await;
        assert!(x2.contains("<Key>b.txt</Key>"));
        assert!(!x2.contains("<Key>a.txt</Key>"));
        assert!(x2.contains("<NextContinuationToken>b.txt</NextContinuationToken>"));

        // Page 3: continue from b.txt → c.txt, done.
        let x3 = list_xml(&state, bucket, v2(1, Some("b.txt".to_string()))).await;
        assert!(x3.contains("<Key>c.txt</Key>"));
        assert!(x3.contains("<IsTruncated>false</IsTruncated>"));
        assert!(!x3.contains("<NextContinuationToken>"));

        let _ = fs::remove_dir_all(storage).await;
    }
}

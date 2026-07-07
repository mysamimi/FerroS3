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

    let mut file_items: Vec<ObjectContent> = Vec::new();
    let mut common_prefixes: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for entry in walkdir::WalkDir::new(storage) {
        // Skip an unreadable entry (e.g. a permission-denied subdir) instead of ending
        // the whole walk, which would silently truncate the listing.
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.file_type().is_dir() {
            continue;
        }

        let rel_path = match entry.path().strip_prefix(storage) {
            Ok(p) => p,
            Err(_) => continue,
        };
        // Build the key from path components joined with '/'. This normalises the
        // separator on Windows without mangling backslashes that are legal in Unix
        // filenames (the old unconditional replace("\\", "/") corrupted such keys).
        let key = rel_path_to_key(rel_path);

        if !key.starts_with(&prefix) {
            continue;
        }

        // Delimiter grouping: collapse keys with a delimiter after the prefix into a
        // CommonPrefix. The BTreeSet de-duplicates.
        if let Some(ref d) = delimiter {
            let relative_to_prefix = &key[prefix.len()..];
            if let Some(idx) = relative_to_prefix.find(d.as_str()) {
                let common_prefix = format!("{}{}{}", prefix, &relative_to_prefix[..idx], d);
                common_prefixes.insert(common_prefix);
                continue;
            }
        }

        // A file that vanished between readdir and stat (e.g. a concurrent DELETE):
        // skip it rather than panicking on unwrap.
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mod_time: DateTime<Utc> = metadata.modified().unwrap_or(SystemTime::now()).into();
        let etag = format!("\"{:x}-{:x}\"", mod_time.timestamp_nanos_opt().unwrap_or(0), metadata.len());

        file_items.push(ObjectContent {
            key,
            last_modified: mod_time.to_rfc3339(),
            etag,
            size: metadata.len(),
            storage_class: "STANDARD".to_string(),
        });
    }

    // Merge files and common prefixes into one key-sorted sequence so results are
    // returned in ascending UTF-8 key order (the S3 guarantee) and pagination is stable.
    enum Item {
        Content(Box<ObjectContent>),
        Prefix(String),
    }
    let mut items: Vec<(String, Item)> =
        Vec::with_capacity(file_items.len() + common_prefixes.len());
    for c in file_items {
        items.push((c.key.clone(), Item::Content(Box::new(c))));
    }
    for p in common_prefixes {
        items.push((p.clone(), Item::Prefix(p)));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));

    // Apply the exclusive start key (marker / continuation-token).
    if let Some(ref start) = start_after {
        items.retain(|(k, _)| k > start);
    }

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
            Item::Content(c) => contents.push(*c),
            Item::Prefix(p) => common_prefixes_out.push(CommonPrefix { prefix: p }),
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
    use dashmap::DashMap;
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
            auth: None,
            buckets: vec![BucketConfig { name: bucket_name.to_string(), storage: storage_path.to_string() }],
        };

        Arc::new(AppState {
            config,
            cache: DashMap::new(),
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

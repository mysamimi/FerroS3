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
    
    let mut contents = Vec::new();
    let mut common_prefixes = std::collections::BTreeSet::new();
    
    let mut walker = walkdir::WalkDir::new(storage).into_iter();

    while let Some(Ok(entry)) = walker.next() {
        if entry.file_type().is_dir() {
            continue;
        }

        let rel_path = entry.path().strip_prefix(storage).unwrap();
        let key = rel_path.to_string_lossy().replace("\\", "/");
        
        if !key.starts_with(&prefix) {
            continue;
        }

        // Handle Delimiter
        if let Some(ref d) = delimiter {
            let relative_to_prefix = &key[prefix.len()..];
            if let Some(idx) = relative_to_prefix.find(d) {
                let common_prefix = format!("{}{}{}", prefix, &relative_to_prefix[..idx], d);
                common_prefixes.insert(common_prefix);
                walker.skip_current_dir(); // Optimization
                continue;
            }
        }

        let metadata = entry.metadata().unwrap();
        let mod_time: DateTime<Utc> = metadata.modified().unwrap_or(SystemTime::now()).into();
        let etag = format!("\"{:x}-{:x}\"", mod_time.timestamp_nanos_opt().unwrap_or(0), metadata.len());

        contents.push(ObjectContent {
            key,
            last_modified: mod_time.to_rfc3339(),
            etag,
            size: metadata.len(),
            storage_class: "STANDARD".to_string(),
        });

        if contents.len() >= max_keys {
            break;
        }
    }

    let result = ListBucketResult {
        xmlns: "http://s3.amazonaws.com/doc/2006-03-01/".to_string(),
        name: bucket,
        prefix,
        marker: params.marker,
        next_marker: None,
        max_keys,
        delimiter,
        is_truncated: false,
        contents,
        common_prefixes: common_prefixes.into_iter().map(|p| CommonPrefix { prefix: p }).collect(),
        key_count: if params.list_type == Some(2) { Some(0) } else { None }, // Simplified
        continuation_token: params.continuation_token,
        next_continuation_token: None,
    };

    let xml = quick_xml::se::to_string(&result).unwrap();
    Response::builder()
        .header(header::CONTENT_TYPE, "application/xml")
        .body(Body::from(xml))
        .unwrap()
}

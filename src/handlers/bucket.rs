use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use std::sync::Arc;
use crate::state::AppState;
use crate::error::S3ErrorType;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
#[serde(rename = "ListAllMyBucketsResult")]
pub struct ListAllMyBucketsResult {
    #[serde(rename = "@xmlns")]
    pub xmlns: String,
    #[serde(rename = "Owner")]
    pub owner: Owner,
    #[serde(rename = "Buckets")]
    pub buckets: Buckets,
}

/// Container so the XML nests as `<Buckets><Bucket>…</Bucket></Buckets>`, which is
/// what S3 clients parse. Without the wrapper each entry serialized as a bare
/// repeated `<Buckets>` element and SDKs saw zero buckets.
#[derive(Serialize, ToSchema)]
pub struct Buckets {
    #[serde(rename = "Bucket")]
    pub bucket: Vec<Bucket>,
}

#[derive(Serialize, ToSchema)]
pub struct Owner {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "DisplayName")]
    pub display_name: String,
}

#[derive(Serialize, ToSchema)]
pub struct Bucket {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "CreationDate")]
    pub creation_date: String,
}

pub async fn list_buckets(
    State(state): State<Arc<AppState>>,
) -> Response {
    let mut buckets = Vec::new();
    for b in &state.config.buckets {
        buckets.push(Bucket {
            name: b.name.clone(),
            creation_date: "2006-02-03T16:45:09.000Z".to_string(), // Constant for now
        });
    }

    let result = ListAllMyBucketsResult {
        xmlns: "http://s3.amazonaws.com/doc/2006-03-01/".to_string(),
        owner: Owner {
            id: "75aa57f09aa0c8caeab4f8c24e99d10f8e7faeebf76c078efc7c6caea54ba06a".to_string(),
            display_name: "Owner".to_string(),
        },
        buckets: Buckets { bucket: buckets },
    };

    let xml = quick_xml::se::to_string(&result).unwrap();
    Response::builder()
        .header(header::CONTENT_TYPE, "application/xml")
        .body(Body::from(xml))
        .unwrap()
}

pub async fn head_bucket(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(bucket): axum::extract::Path<String>,
) -> Response {
    if state.storage_map.contains_key(&bucket) {
        StatusCode::OK.into_response()
    } else {
        S3ErrorType::NoSuchBucket.to_response(Some(bucket))
    }
}

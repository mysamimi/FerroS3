use axum::{
    extract::{State, Query},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use std::sync::Arc;
use chrono::Utc;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use crate::state::AppState;
use utoipa::IntoParams;

type HmacSha256 = Hmac<Sha256>;

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PresignParams {
    /// Target bucket name.
    pub bucket: String,
    /// Object key to sign.
    pub key: String,
    /// HTTP method to allow for the presigned URL.
    pub method: String,
    /// Expiration time in seconds.
    pub expires: u64,
}

pub async fn generate_presigned_url(
    State(state): State<Arc<AppState>>,
    Query(params): Query<PresignParams>,
) -> impl IntoResponse {
    let auth_cfg = match &state.config.auth {
        Some(a) => a,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "Auth not configured").into_response(),
    };

    let now = Utc::now();
    let date_stamp = now.format("%Y%m%d").to_string();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let region = "us-east-1"; // Default
    let service = "s3";
    
    let scope = format!("{}/{}/{}/aws4_request", date_stamp, region, service);
    let credential = format!("{}/{}", auth_cfg.access_key, scope);
    
    let mut query_params = std::collections::BTreeMap::new();
    query_params.insert("X-Amz-Algorithm".to_string(), "AWS4-HMAC-SHA256".to_string());
    query_params.insert("X-Amz-Credential".to_string(), credential);
    query_params.insert("X-Amz-Date".to_string(), amz_date.clone());
    query_params.insert("X-Amz-Expires".to_string(), params.expires.to_string());
    query_params.insert("X-Amz-SignedHeaders".to_string(), "host".to_string());

    // Canonical Request for Presigned URL
    let canonical_uri = format!("/{}/{}", params.bucket, params.key);
    let mut canonical_query = Vec::new();
    for (k, v) in &query_params {
        canonical_query.push(format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)));
    }
    let canonical_query_string = canonical_query.join("&");
    
    let host = format!("{}:{}", state.config.endpoint, state.config.port);
    let canonical_headers = format!("host:{}\n", host);
    let signed_headers = "host";
    let payload_hash = "UNSIGNED-PAYLOAD";
    
    let mut hasher = Sha256::new();
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        params.method.to_uppercase(),
        canonical_uri,
        canonical_query_string,
        canonical_headers,
        signed_headers,
        payload_hash
    );
    hasher.update(canonical_request.as_bytes());
    let canonical_request_hash = hex::encode(hasher.finalize());
    
    // String to Sign
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        amz_date,
        scope,
        canonical_request_hash
    );
    
    // Signing Key
    let k_secret = format!("AWS4{}", auth_cfg.secret_key);
    let k_date = hmac_sha256(k_secret.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    
    let mut mac = HmacSha256::new_from_slice(&k_signing).unwrap();
    mac.update(string_to_sign.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());
    
    let url = format!(
        "http://{}{}?{}&X-Amz-Signature={}",
        host,
        canonical_uri,
        canonical_query_string,
        signature
    );
    
    url.into_response()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

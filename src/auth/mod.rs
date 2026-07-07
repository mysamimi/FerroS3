pub mod sigv4;

use axum::{
    extract::{State, Request},
    http::header,
    middleware::Next,
    response::Response,
};
use base64::Engine;
use std::sync::Arc;
use std::collections::BTreeMap;
use crate::state::AppState;
use crate::auth::sigv4::{constant_time_eq, verify_signature, SigV4Params};
use crate::error::S3ErrorType;

pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    if state.config.verbose {
        // Log the path only — the query string carries X-Amz-Signature / X-Amz-Credential,
        // which must not be written to logs.
        println!("--> {} {}", req.method(), req.uri().path());
    }

    let auth_header = req.headers().get(header::AUTHORIZATION).and_then(|h| h.to_str().ok()).map(|s| s.to_string());
    let query_string = req.uri().query().unwrap_or_default().to_string();
    let query_params = parse_query(&query_string);

    if let Some(auth) = auth_header {
        if auth.starts_with("AWS4-HMAC-SHA256") {
            return verify_header_auth(&state, req, &auth, &query_params, next).await;
        }

        if let Some((access_key, secret_key)) = parse_basic_auth(&auth) {
            if let Some(auth_cfg) = &state.config.auth {
                // Compare the secret in constant time to avoid a timing side channel;
                // the access key is public, so a plain compare is fine.
                if auth_cfg.access_key == access_key
                    && constant_time_eq(auth_cfg.secret_key.as_bytes(), secret_key.as_bytes())
                {
                    return next.run(req).await;
                }
            }
        }
    }

    if query_params.contains_key("X-Amz-Signature") {
        return verify_query_auth(&state, req, &query_params, next).await;
    }

    // Default to unauthorized if auth is configured but missing
    if state.config.auth.is_some() {
        if state.config.verbose { println!("  [!] Missing authentication"); }
        return S3ErrorType::AccessDenied.to_response(None);
    }

    next.run(req).await
}

async fn verify_header_auth(
    state: &AppState,
    req: Request,
    auth: &str,
    query: &BTreeMap<String, String>,
    next: Next,
) -> Response {
    // Example: AWS4-HMAC-SHA256 Credential=AKIA.../20240516/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=...
    let parts: Vec<&str> = auth.split(',').collect();
    if parts.len() < 3 { return S3ErrorType::AccessDenied.to_response(None); }

    let credential_part = parts[0].split('=').nth(1).unwrap_or("");
    let cred_subparts: Vec<&str> = credential_part.split('/').collect();
    if cred_subparts.len() < 5 { return S3ErrorType::AccessDenied.to_response(None); }

    let access_key = cred_subparts[0];
    let _date_short = cred_subparts[1];
    let region = cred_subparts[2];
    let service = cred_subparts[3];

    let signed_headers_part = parts[1].split('=').nth(1).unwrap_or("");
    let signature = parts[2].trim().split('=').nth(1).unwrap_or("");

    let auth_cfg = match &state.config.auth {
        Some(a) => a,
        None => return next.run(req).await,
    };

    if auth_cfg.access_key != access_key { return S3ErrorType::AccessDenied.to_response(None); }

    let x_amz_date = match req.headers().get("x-amz-date").and_then(|h| h.to_str().ok()) {
        Some(d) => d,
        None => return S3ErrorType::AccessDenied.to_response(None),
    };

    let x_amz_content_sha256 = req.headers().get("x-amz-content-sha256").and_then(|h| h.to_str().ok()).unwrap_or("UNSIGNED-PAYLOAD");

    let mut signed_headers = BTreeMap::new();
    for h in signed_headers_part.trim().split(';') {
        if let Some(val) = req.headers().get(h).and_then(|v| v.to_str().ok()) {
            signed_headers.insert(h.to_string(), val.to_string());
        }
    }

    let params = SigV4Params {
        method: req.method().as_str(),
        path: req.uri().path(),
        query,
        headers: &signed_headers,
        payload_hash: x_amz_content_sha256,
        _access_key: access_key,
        secret_key: &auth_cfg.secret_key,
        region,
        service,
        date: x_amz_date,
    };

    if verify_signature(params, signature) {
        next.run(req).await
    } else {
        if state.config.verbose { println!("  [!] SigV4 Header Verification Failed"); }
        S3ErrorType::AccessDenied.to_response(None)
    }
}

async fn verify_query_auth(
    state: &AppState,
    req: Request,
    query: &BTreeMap<String, String>,
    next: Next,
) -> Response {
    let access_key_full = match query.get("X-Amz-Credential") {
        Some(c) => c,
        None => return S3ErrorType::AccessDenied.to_response(None),
    };
    let cred_parts: Vec<&str> = access_key_full.split('/').collect();
    if cred_parts.len() < 5 { return S3ErrorType::AccessDenied.to_response(None); }

    let access_key = cred_parts[0];
    let region = cred_parts[2];
    let service = cred_parts[3];
    let x_amz_date = match query.get("X-Amz-Date") {
        Some(d) => d,
        None => return S3ErrorType::AccessDenied.to_response(None),
    };
    let signature = match query.get("X-Amz-Signature") {
        Some(s) => s,
        None => return S3ErrorType::AccessDenied.to_response(None),
    };
    let signed_headers_list = match query.get("X-Amz-SignedHeaders") {
        Some(h) => h,
        None => return S3ErrorType::AccessDenied.to_response(None),
    };

    // Enforce presigned-URL expiry. A presigned URL is a bearer credential valid only
    // for X-Amz-Expires seconds after X-Amz-Date; without this check it never expires.
    let expires: i64 = match query.get("X-Amz-Expires").and_then(|e| e.parse().ok()) {
        Some(e) if e > 0 && e <= 604_800 => e, // AWS caps presign lifetime at 7 days
        _ => return S3ErrorType::AccessDenied.to_response(None),
    };
    match chrono::NaiveDateTime::parse_from_str(x_amz_date, "%Y%m%dT%H%M%SZ") {
        Ok(signed_at) => {
            let age = chrono::Utc::now()
                .signed_duration_since(signed_at.and_utc())
                .num_seconds();
            // Reject expired URLs, and URLs dated too far in the future (allow small skew).
            if age > expires || age < -300 {
                return S3ErrorType::AccessDenied.to_response(None);
            }
        }
        Err(_) => return S3ErrorType::AccessDenied.to_response(None),
    }

    let auth_cfg = match &state.config.auth {
        Some(a) => a,
        None => return next.run(req).await,
    };

    if auth_cfg.access_key != access_key { return S3ErrorType::AccessDenied.to_response(None); }

    let mut signed_headers = BTreeMap::new();
    for h in signed_headers_list.split(';') {
        if let Some(val) = req.headers().get(h).and_then(|v| v.to_str().ok()) {
            signed_headers.insert(h.to_string(), val.to_string());
        }
    }

    let params = SigV4Params {
        method: req.method().as_str(),
        path: req.uri().path(),
        query,
        headers: &signed_headers,
        payload_hash: "UNSIGNED-PAYLOAD",
        _access_key: access_key,
        secret_key: &auth_cfg.secret_key,
        region,
        service,
        date: x_amz_date,
    };

    if verify_signature(params, signature) {
        next.run(req).await
    } else {
        if state.config.verbose { println!("  [!] SigV4 Query Verification Failed"); }
        S3ErrorType::AccessDenied.to_response(None)
    }
}

fn parse_query(query: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for part in query.split('&') {
        if part.is_empty() {
            continue;
        }
        // Split on the FIRST '=' only: a valueless flag (e.g. `acl`) keeps an empty
        // value, and a value containing '=' (e.g. a base64 token) is preserved. The
        // previous `split('=')` with a len==2 guard dropped both, which broke SigV4
        // canonical-query construction for those requests.
        let (key, value) = match part.split_once('=') {
            Some((k, v)) => (k, v),
            None => (part, ""),
        };
        map.insert(
            key.to_string(),
            urlencoding::decode(value).unwrap_or_default().into_owned(),
        );
    }
    map
}

fn parse_basic_auth(auth: &str) -> Option<(String, String)> {
    let encoded = auth.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (access_key, secret_key) = decoded.split_once(':')?;
    Some((access_key.to_string(), secret_key.to_string()))
}

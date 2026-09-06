pub mod auth;
pub mod blocking_http;
pub mod cache;
pub mod config;
pub mod error;
pub mod handlers;
pub mod openapi;
pub mod state;

use axum::{
    extract::{Request, State},
    middleware::{self, Next},
    response::Response,
    routing::get,
    Router,
};
use quick_cache::sync::Cache;
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::fs;

use crate::auth::auth_middleware;
use crate::error::S3ErrorType;
use crate::config::Config;
use crate::handlers::admin::generate_presigned_url;
use crate::handlers::bucket::{head_bucket, list_buckets};
use crate::handlers::list::list_objects;
use crate::handlers::object::{delete_object, get_object, head_object, put_object};
use crate::state::AppState;

/// One-line build stamp, printed at startup and the first thing to check when a server
/// misbehaves: it ties the running process back to the revision it was built from.
/// The values are baked in by build.rs; any of them can read "unknown" if the build
/// happened outside a git checkout.
pub fn build_stamp() -> String {
    let built = chrono::DateTime::from_timestamp(
        env!("FERROS3_BUILD_EPOCH").parse::<i64>().unwrap_or(0),
        0,
    )
    .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
    .unwrap_or_else(|| "unknown".to_string());

    format!(
        "ferros3 {} | version {} | commit {} | built {}",
        env!("CARGO_PKG_VERSION"),
        env!("FERROS3_GIT_DESCRIBE"),
        env!("FERROS3_GIT_COMMIT"),
        built,
    )
}

pub async fn load_config() -> Config {
    let config_path = "config.yaml";
    let config_str = fs::read_to_string(config_path)
        .await
        .expect("Failed to read config.yaml");
    serde_yaml::from_str(&config_str).expect("Failed to parse config.yaml")
}

pub fn build_state(config: &Config) -> Arc<AppState> {
    let mut storage_map = HashMap::new();
    for bucket in &config.buckets {
        storage_map.insert(bucket.name.clone(), PathBuf::from(&bucket.storage));
    }

    Arc::new(AppState {
        config: config.clone(),
        // `cache_size` is the cache's max entry count (it was previously only an
        // initial capacity, so the cache grew without bound).
        cache: Cache::new(config.cache_size),
        storage_map,
    })
}

pub fn build_api_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_buckets))
        .route("/_admin/presign", axum::routing::post(generate_presigned_url))
        // S3 path-style clients address a bucket as `/bucket` (no trailing slash);
        // register both spellings so ListObjects/HeadBucket don't 404.
        .route("/:bucket", get(list_objects).head(head_bucket))
        .route("/:bucket/", get(list_objects).head(head_bucket))
        .route(
            "/:bucket/*key",
            get(get_object)
                .head(head_object)
                .put(put_object)
                .delete(delete_object),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        // Outermost, so it also bounds auth: the last layer added wraps everything
        // registered before it.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            timeout_middleware,
        ))
        .with_state(state)
}

/// Bound how long a request may take to produce a response, so a hung storage mount
/// (an NFS server that stopped answering, a disk in permanent I/O wait) fails the
/// request instead of holding the connection open forever.
///
/// Two things are deliberately outside the bound:
/// * Requests carrying a body (PUT/POST). The handler consumes the upload inside this
///   future, so the elapsed time is the client's upload speed — a legitimately slow
///   1 GiB PUT must not be cut off.
/// * Streaming the response body. The future resolves once the response head and body
///   stream are built, so a slow client downloading a large object is unaffected.
///
/// A timed-out request is abandoned, not cancelled: a `spawn_blocking` walk already
/// stuck in a syscall keeps running on its blocking thread until the filesystem
/// answers. The bound protects the client and the connection, not the thread pool.
async fn timeout_middleware(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let seconds = state.config.request_timeout_secs;
    let has_body = matches!(*req.method(), axum::http::Method::PUT | axum::http::Method::POST);
    if seconds == 0 || has_body {
        return next.run(req).await;
    }

    let verbose = state.config.verbose;
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    match tokio::time::timeout(Duration::from_secs(seconds), next.run(req)).await {
        Ok(response) => response,
        Err(_) => {
            if verbose {
                println!("  [!] Timed out after {seconds}s: {method} {path}");
            }
            S3ErrorType::RequestTimeout.to_response(None)
        }
    }
}

#[cfg(debug_assertions)]
pub fn build_docs_router() -> Router {
    Router::new()
        .route("/openapi.json", get(crate::openapi::openapi_json))
        .route("/docs", get(crate::openapi::swagger_ui_html))
        .route("/docs/", get(crate::openapi::swagger_ui_html))
}

pub fn build_app(state: Arc<AppState>) -> Router {
    let app = Router::new().merge(build_api_router(state));

    #[cfg(debug_assertions)]
    let app = app.merge(build_docs_router());

    app
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CachedStat;
    use crate::config::BucketConfig;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::put;
    use chrono::Utc;
    use tower::ServiceExt;

    fn timeout_state(seconds: u64) -> Arc<AppState> {
        build_state(&Config {
            port: 0,
            endpoint: String::new(),
            verbose: false,
            cache_size: 8,
            fsync: true,
            request_timeout_secs: seconds,
            auth: None,
            buckets: Vec::<BucketConfig>::new(),
        })
    }

    /// A router whose only route sleeps for 5s behind the timeout middleware. Tests run
    /// with a paused clock, so the sleep is virtual and the assertion is instant.
    fn slow_app(state: Arc<AppState>, method: &str) -> Router {
        let slow = || async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            "done"
        };
        let route = if method == "PUT" { put(slow) } else { get(slow) };
        Router::new()
            .route("/slow", route)
            .layer(middleware::from_fn_with_state(state.clone(), timeout_middleware))
            .with_state(state)
    }

    async fn status_of(app: Router, method: &str) -> StatusCode {
        let request = Request::builder()
            .method(method)
            .uri("/slow")
            .body(Body::empty())
            .unwrap();
        app.oneshot(request).await.unwrap().status()
    }

    #[tokio::test(start_paused = true)]
    async fn slow_request_times_out_instead_of_hanging() {
        // A handler stuck on unresponsive storage must not hold the connection open.
        let state = timeout_state(1);
        assert_eq!(
            status_of(slow_app(state, "GET"), "GET").await,
            StatusCode::GATEWAY_TIMEOUT
        );
    }

    #[tokio::test(start_paused = true)]
    async fn zero_timeout_disables_the_bound() {
        let state = timeout_state(0);
        assert_eq!(status_of(slow_app(state, "GET"), "GET").await, StatusCode::OK);
    }

    #[tokio::test(start_paused = true)]
    async fn uploads_are_exempt_from_the_timeout() {
        // A PUT's duration is the client's upload speed; cutting it off would break
        // legitimately slow large uploads.
        let state = timeout_state(1);
        assert_eq!(status_of(slow_app(state, "PUT"), "PUT").await, StatusCode::OK);
    }

    #[test]
    fn stat_cache_is_bounded_by_cache_size() {
        let config = Config {
            port: 0,
            endpoint: String::new(),
            verbose: false,
            cache_size: 8,
            fsync: true,
            request_timeout_secs: 30,
            auth: None,
            buckets: vec![],
        };
        let state = build_state(&config);

        // Insert far more entries than the bound; eviction must keep the cache at or
        // under cache_size (the old DashMap grew without limit here).
        for i in 0..1000 {
            state.cache.insert(
                format!("bucket/key-{i}"),
                CachedStat { size: i, mod_time: Utc::now(), etag: format!("\"{i}\"") },
            );
        }
        assert!(
            state.cache.len() <= 8,
            "cache exceeded its bound: {} entries",
            state.cache.len()
        );
    }
}

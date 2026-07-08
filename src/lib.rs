pub mod auth;
pub mod blocking_http;
pub mod cache;
pub mod config;
pub mod error;
pub mod handlers;
pub mod openapi;
pub mod state;

use axum::{middleware, routing::get, Router};
use quick_cache::sync::Cache;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::fs;

use crate::auth::auth_middleware;
use crate::config::Config;
use crate::handlers::admin::generate_presigned_url;
use crate::handlers::bucket::{head_bucket, list_buckets};
use crate::handlers::list::list_objects;
use crate::handlers::object::{delete_object, get_object, head_object, put_object};
use crate::state::AppState;

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
        .with_state(state)
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
    use chrono::Utc;

    #[test]
    fn stat_cache_is_bounded_by_cache_size() {
        let config = Config {
            port: 0,
            endpoint: String::new(),
            verbose: false,
            cache_size: 8,
            fsync: true,
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

mod config;
mod cache;
mod state;
mod error;
mod handlers;
mod auth;
mod openapi;

use axum::{
    middleware,
    routing::get,
    Router,
};
use dashmap::DashMap;
use std::{
    collections::HashMap,
    path::{PathBuf},
    sync::Arc,
};
use tokio::fs;

use crate::config::Config;
use crate::state::AppState;
use crate::handlers::object::{get_object, head_object, put_object, delete_object};
use crate::handlers::list::list_objects;
use crate::handlers::bucket::{head_bucket, list_buckets};
use crate::handlers::admin::generate_presigned_url;
use crate::auth::auth_middleware;

#[cfg(target_os = "freebsd")]
use axum::body::{to_bytes, Body};
#[cfg(target_os = "freebsd")]
use axum::response::IntoResponse;
#[cfg(target_os = "freebsd")]
use http::{HeaderName, HeaderValue, Method, Request, StatusCode};
#[cfg(target_os = "freebsd")]
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::TcpStream as StdTcpStream,
};
#[cfg(target_os = "freebsd")]
use tower::ServiceExt;

async fn load_config() -> Config {
    let config_path = "config.yaml";
    let config_str = fs::read_to_string(config_path)
        .await
        .expect("Failed to read config.yaml");
    serde_yaml::from_str(&config_str).expect("Failed to parse config.yaml")
}

fn build_state(config: &Config) -> Arc<AppState> {
    let mut storage_map = HashMap::new();
    for b in &config.buckets {
        storage_map.insert(b.name.clone(), PathBuf::from(&b.storage));
    }

    Arc::new(AppState {
        config: config.clone(),
        cache: DashMap::with_capacity(config.cache_size),
        storage_map,
    })
}

fn build_api_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_buckets))
        .route("/_admin/presign", axum::routing::post(generate_presigned_url))
        .route("/:bucket/", get(list_objects).head(head_bucket))
        .route("/:bucket/*key", get(get_object).head(head_object).put(put_object).delete(delete_object))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .with_state(state)
}

#[cfg(debug_assertions)]
fn build_docs_router() -> Router {
    Router::new()
        .route("/openapi.json", get(crate::openapi::openapi_json))
        .route("/docs", get(crate::openapi::swagger_ui_html))
        .route("/docs/", get(crate::openapi::swagger_ui_html))
}

fn build_app(state: Arc<AppState>) -> Router {
    let app = Router::new().merge(build_api_router(state));

    #[cfg(debug_assertions)]
    let app = app.merge(build_docs_router());

    app
}

#[cfg(not(target_os = "freebsd"))]
#[tokio::main]
async fn main() {
    let config = load_config().await;
    let state = build_state(&config);
    let app = build_app(state);

    let addr = format!("{}:{}", config.endpoint, config.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind TCP listener");
    println!("Rust S3 Proxy listening on http://{}", addr);

    axum::serve(listener, app)
        .await
        .expect("server error");
}

#[cfg(target_os = "freebsd")]
#[tokio::main]
async fn main() {
    let config = load_config().await;
    let state = build_state(&config);
    let app = build_app(state);

    let addr = format!("{}:{}", config.endpoint, config.port);
    let listener = std::net::TcpListener::bind(&addr).expect("Failed to bind TCP listener");
    println!("Rust S3 Proxy listening on http://{}", addr);

    let runtime_handle = tokio::runtime::Handle::current();

    loop {
        let (stream, peer) = match listener.accept() {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("Failed to accept socket connection: {:?}", e);
                continue;
            }
        };

        let app = app.clone();
        let handle = runtime_handle.clone();
        std::thread::spawn(move || {
            if let Err(e) = serve_blocking_connection(stream, peer, app, handle) {
                eprintln!("Connection error: {:?}", e);
            }
        });
    }
}

#[cfg(target_os = "freebsd")]
fn serve_blocking_connection(
    mut stream: StdTcpStream,
    _peer: std::net::SocketAddr,
    app: Router,
    handle: tokio::runtime::Handle,
) -> io::Result<()> {
    stream.set_nodelay(true).ok();
    let (request, method) = read_http_request(&mut stream)?;

    let (status, headers, body_bytes) = handle.block_on(async move {
        let response = app
            .oneshot(request)
            .await
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        let (parts, body) = response.into_parts();
        let body_bytes = to_bytes(body, usize::MAX).await.unwrap_or_default();
        (parts.status, parts.headers, body_bytes)
    });

    write_http_response(&mut stream, status, headers, body_bytes, method == Method::HEAD)
}

#[cfg(target_os = "freebsd")]
fn read_http_request(stream: &mut StdTcpStream) -> io::Result<(Request<Body>, Method)> {
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    if request_line.trim().is_empty() {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "empty request"));
    }

    let mut parts = request_line.trim_end_matches(['\r', '\n']).split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing method"))?;
    let target = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing target"))?;

    let method = Method::from_bytes(method.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid method"))?;

    let mut headers = axum::http::HeaderMap::new();
    let mut content_length = 0usize;

    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }

        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed header"))?;

        let header_name = HeaderName::from_bytes(name.trim().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid header name"))?;
        let header_value = HeaderValue::from_str(value.trim())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid header value"))?;

        if header_name == http::header::CONTENT_LENGTH {
            content_length = header_value
                .to_str()
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
        }

        headers.append(header_name, header_value);
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    let mut builder = Request::builder().method(method.clone()).uri(target);
    if let Some(headers_mut) = builder.headers_mut() {
        *headers_mut = headers;
    }

    let request = builder
        .body(Body::from(body))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "failed to build request"))?;

    Ok((request, method))
}

#[cfg(target_os = "freebsd")]
fn write_http_response(
    stream: &mut StdTcpStream,
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body_bytes: bytes::Bytes,
    head_only: bool,
) -> io::Result<()> {
    let reason = status.canonical_reason().unwrap_or("");

    let mut response_head = format!("HTTP/1.1 {} {}\r\n", status.as_u16(), reason);
    response_head.push_str("Connection: close\r\n");
    response_head.push_str(&format!("Content-Length: {}\r\n", body_bytes.len()));

    for (name, value) in headers.iter() {
        if name == http::header::CONTENT_LENGTH || name == http::header::CONNECTION {
            continue;
        }
        if let Ok(value_str) = value.to_str() {
            response_head.push_str(name.as_str());
            response_head.push_str(": ");
            response_head.push_str(value_str);
            response_head.push_str("\r\n");
        }
    }

    response_head.push_str("\r\n");
    stream.write_all(response_head.as_bytes())?;
    if !head_only {
        stream.write_all(&body_bytes)?;
    }
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Request, StatusCode};
    use tower::util::ServiceExt;
    use crate::config::{BucketConfig, AuthConfig};
    use axum::body::Body;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_storage_dir(test_name: &str) -> PathBuf {
        let unique_suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        PathBuf::from(format!("./test_data_{}_{}", test_name, unique_suffix))
    }

    fn setup_test_state(storage_dir: &PathBuf) -> Arc<AppState> {
        let config = Config {
            port: 8080,
            endpoint: "0.0.0.0".to_string(),
            verbose: false,
            cache_size: 10,
            auth: Some(AuthConfig {
                access_key: "test_key".to_string(),
                secret_key: "test_secret".to_string(),
            }),
            buckets: vec![BucketConfig {
                name: "test_bucket".to_string(),
                storage: storage_dir.to_string_lossy().to_string(),
            }],
        };

        let mut storage_map = HashMap::new();
        storage_map.insert("test_bucket".to_string(), storage_dir.clone());

        Arc::new(AppState {
            config,
            cache: DashMap::new(),
            storage_map,
        })
    }

    #[tokio::test]
    async fn test_auth_failure() {
        let storage_dir = test_storage_dir("auth_failure");
        let state = setup_test_state(&storage_dir);
        let app = Router::new()
            .route("/:bucket/", get(list_objects))
            .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
            .with_state(state);

        let response = app
            .oneshot(Request::builder().uri("/test_bucket/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_auth_success() {
        let storage_dir = test_storage_dir("auth_success");
        let state = setup_test_state(&storage_dir);
        let app = Router::new()
            .route("/:bucket/", get(list_objects))
            .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
            .with_state(state);

        // Create test directory if not exists
        fs::create_dir_all(&storage_dir).await.unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/test_bucket/")
                    .header("Authorization", "test_key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        
        // Clean up
        let _ = fs::remove_dir_all(&storage_dir).await;
    }

    #[tokio::test]
    async fn test_put_and_delete_object() {
        let storage_dir = test_storage_dir("put_and_delete_object");
        let state = setup_test_state(&storage_dir);
        let app = Router::new()
            .route("/:bucket/*key", get(get_object).put(put_object).delete(delete_object))
            .with_state(state);

        fs::create_dir_all(&storage_dir).await.unwrap();

        // 1. Put Object
        let response = app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/test_bucket/new_file.txt")
                    .body(Body::from("hello world"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Verify file exists
        assert!(fs::metadata(storage_dir.join("new_file.txt")).await.is_ok());

        // 2. Delete Object
        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/test_bucket/new_file.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // Verify file is gone
        assert!(fs::metadata(storage_dir.join("new_file.txt")).await.is_err());

        // Clean up
        let _ = fs::remove_dir_all(&storage_dir).await;
    }

    #[tokio::test]
    async fn test_openapi_route_is_available() {
        let storage_dir = test_storage_dir("openapi_route");
        let app = build_app(setup_test_state(&storage_dir));

        let response = app
            .oneshot(Request::builder().uri("/openapi.json").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}

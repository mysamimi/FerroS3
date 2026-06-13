use ferros3::{build_app, build_state, load_config};

#[cfg(target_os = "freebsd")]
use axum::body::{to_bytes, Body};
#[cfg(target_os = "freebsd")]
use axum::Router;
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

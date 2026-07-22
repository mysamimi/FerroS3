use ferros3::{build_app, build_state, load_config};

#[cfg(target_os = "freebsd")]
use axum::body::Body;
#[cfg(target_os = "freebsd")]
use axum::http::{Method, Request, StatusCode};
#[cfg(target_os = "freebsd")]
use axum::response::IntoResponse;
#[cfg(target_os = "freebsd")]
use axum::Router;
#[cfg(target_os = "freebsd")]
use ferros3::blocking_http::{
    body_plan, format_response_head, parse_request_head, read_body, request_keep_alive,
    wants_100_continue, MAX_BODY_BYTES,
};
#[cfg(target_os = "freebsd")]
use http_body_util::BodyExt;
#[cfg(target_os = "freebsd")]
use hyper::body::Body as HttpBody;
#[cfg(target_os = "freebsd")]
use std::{
    io::{self, BufReader, Write},
    net::TcpStream as StdTcpStream,
    sync::atomic::{AtomicUsize, Ordering},
};
#[cfg(target_os = "freebsd")]
use tower::ServiceExt;

/// Cap on concurrent connection threads, so a flood can't spawn unbounded OS threads
/// (a pre-auth resource-exhaustion vector).
#[cfg(target_os = "freebsd")]
const MAX_CONNECTIONS: usize = 512;

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
    static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

    loop {
        let (stream, peer) = match listener.accept() {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("Failed to accept socket connection: {:?}", e);
                continue;
            }
        };

        // Shed load past the connection cap instead of spawning threads without bound.
        if ACTIVE_CONNECTIONS.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
            drop(stream);
            continue;
        }
        ACTIVE_CONNECTIONS.fetch_add(1, Ordering::Relaxed);

        let app = app.clone();
        let handle = runtime_handle.clone();
        std::thread::spawn(move || {
            if let Err(e) = serve_blocking_connection(stream, peer, app, handle) {
                eprintln!("Connection error: {:?}", e);
            }
            ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
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

    // One reader for the connection's whole lifetime: bytes it buffers past one
    // request's body are the start of the next request, and a per-request reader
    // would silently discard them.
    let mut reader = BufReader::new(stream.try_clone()?);

    // Keep-alive loop: serve requests until the client closes, asks to close, or a
    // response's length can't be framed for reuse.
    loop {
        let head = match parse_request_head(&mut reader) {
            Ok(head) => head,
            // Client closed the connection between requests: a normal end, not an error.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        let head_only = head.method == Method::HEAD;
        let client_keep_alive = request_keep_alive(head.http11, &head.headers);

        // Honor `Expect: 100-continue` before reading the body, or standard clients
        // stall waiting for the interim response.
        if wants_100_continue(&head.headers) {
            stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            stream.flush()?;
        }

        // Read the body per its framing (Content-Length or chunked), bounded so a huge
        // declared length can't pre-allocate gigabytes.
        let body = read_body(&mut reader, body_plan(&head.headers), MAX_BODY_BYTES)?;

        let mut builder = Request::builder().method(head.method).uri(head.target);
        if let Some(headers_mut) = builder.headers_mut() {
            *headers_mut = head.headers;
        }
        let request = builder
            .body(Body::from(body))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "failed to build request"))?;

        let response = handle
            .block_on(app.clone().oneshot(request))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        let (parts, mut body) = response.into_parts();

        // A body with an exact size hint (every handler response: file streams sized by
        // Content-Length/take, full buffers, empties) can be framed for keep-alive. An
        // unknown length falls back to close-delimited framing.
        let exact_len = HttpBody::size_hint(&body).exact();
        let keep_alive = client_keep_alive && (head_only || exact_len.is_some());

        let response_head =
            format_response_head(parts.status, &parts.headers, exact_len, head_only, keep_alive);
        stream.write_all(response_head.as_bytes())?;

        if !head_only {
            // Stream body frames to the socket as they arrive, instead of buffering the
            // whole response in memory (the old to_bytes held up to 2 GiB per connection).
            handle.block_on(async {
                while let Some(frame) = body.frame().await {
                    let frame = frame.map_err(|_| {
                        // Mid-stream failure after the head is sent: the head (status,
                        // framing) is already on the wire, so the only honest signal
                        // left is dropping the connection.
                        io::Error::new(io::ErrorKind::Other, "response body stream error")
                    })?;
                    if let Some(data) = frame.data_ref() {
                        stream.write_all(data)?;
                    }
                }
                Ok::<_, io::Error>(())
            })?;
        }
        stream.flush()?;

        if !keep_alive {
            return Ok(());
        }
    }
}

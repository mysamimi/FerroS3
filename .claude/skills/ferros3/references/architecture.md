# Architecture

## Startup

`main` prints `build_stamp()` first, then `load_config()` → `build_state()` → `build_app()`.

- `load_config` reads **`config.yaml` as a relative path** and panics if it is missing or
  malformed. This is why every packaging artifact sets a working directory: the systemd
  unit uses `WorkingDirectory=/app`, and the rc.d script `cd`s to `${ferros3_dir}` before
  exec'ing, because `daemon(8)` on FreeBSD 11 cannot set one.
- `build_state` turns the bucket list into `storage_map: HashMap<bucket, PathBuf>` and
  allocates the stat cache with `cache_size` as a **hard entry bound** (`quick_cache`,
  S3-FIFO eviction). It was once a `DashMap` and grew without limit; the test
  `stat_cache_is_bounded_by_cache_size` guards against a regression.
- `build_app` merges the API router and, **only in debug builds**, the docs router. A
  release binary has no `/docs` and no `/openapi.json`.

## Routing

```rust
"/"                  GET  list_buckets
"/_admin/presign"    POST generate_presigned_url
"/:bucket"           GET  list_objects   HEAD head_bucket
"/:bucket/"          GET  list_objects   HEAD head_bucket
"/:bucket/*key"      GET  get_object  HEAD head_object  PUT put_object  DELETE delete_object
```

Both `/:bucket` and `/:bucket/` are registered because path-style S3 clients address a
bucket without a trailing slash, and axum treats those as distinct routes. Dropping either
spelling 404s ListObjects/HeadBucket for half the clients in the wild.

`/_admin/presign` sits **inside** the auth layer: minting a presigned URL requires
credentials.

## Middleware order

```rust
.layer(auth_middleware)     // added first  → runs second (inner)
.layer(timeout_middleware)  // added last   → runs first  (outer)
```

axum wraps in reverse registration order. The timeout is outermost deliberately, so a
hang inside auth is also bounded.

`timeout_middleware` (in `lib.rs`) skips the bound entirely when
`request_timeout_secs == 0` or the method is PUT/POST. It only bounds the time to *produce
a response* — once the response head and body stream exist, a slow download is unaffected.
A timed-out request is **abandoned, not cancelled**: a `spawn_blocking` walk already stuck
in a syscall keeps occupying its blocking thread until the filesystem answers. The bound
protects the client and the connection, not the thread pool.

## The two servers

**Everywhere except FreeBSD**: `axum::serve` on a tokio `TcpListener`. Ordinary async.

**On FreeBSD** (`#[cfg(target_os = "freebsd")]` in `main.rs`): a blocking `std` listener,
one OS thread per connection, capped at `MAX_CONNECTIONS = 512` (past the cap the socket is
dropped — load shedding, so a pre-auth flood can't spawn unbounded threads). Each thread:

1. parses the head with `parse_request_head` (one `BufReader` for the whole connection —
   a per-request reader would silently discard pipelined bytes it had already buffered),
2. answers `Expect: 100-continue` before reading the body,
3. reads the body per `body_plan` (chunked beats Content-Length, per RFC 7230), bounded by
   `MAX_BODY_BYTES` = 256 MiB — so **on FreeBSD an upload is fully buffered in memory**,
   unlike the streaming async path,
4. drives the axum `Router` through `handle.block_on(app.oneshot(request))`,
5. streams response frames straight to the socket instead of buffering the whole body,
6. decides keep-alive: only when the client allows it *and* the body has an exact size
   hint (or it is a HEAD). Unknown length falls back to close-delimited framing.

All of the parsing/formatting logic lives in `blocking_http.rs` **outside** the cfg gate,
so it compiles and unit-tests on macOS and Linux even though only FreeBSD runs the socket
glue. Keep it that way — that is the only reason those paths have test coverage at all.

## The stat cache

`Cache<String, CachedStat>` keyed by `"{bucket}/{key}"`, holding size, mtime and ETag.

- `head_object` reads it, and populates it on a miss.
- `put_object` removes the entry *after* the rename lands, then primes it with fresh
  metadata (the upload-then-HEAD pattern is common enough to be worth it).
- `copy_object` and `delete_object` invalidate the destination key.
- `get_object` and listing do **not** consult it — they need the file handle / full walk
  anyway.

There is no cross-process invalidation. Anything that writes to the storage directories
behind FerroS3's back can be served stale metadata for as long as the entry survives.

## Error model

Handlers return `axum::response::Response` and early-return on failure with
`S3ErrorType::X.to_response(Some(resource))`, which serialises S3's XML `<Error>` shape
with a random `RequestId`. The id is read from `/dev/urandom` and formatted as a UUIDv4 by
hand, because the `uuid`/`getrandom` path uses a syscall FreeBSD 11 lacks.

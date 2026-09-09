---
name: ferros3
description: Working knowledge of FerroS3, the Rust/axum S3-compatible filesystem proxy in this repo — code map, invariants that must not regress, and the build/test/cross-compile/deploy workflows. Use when changing handlers, auth/SigV4, ListObjects, config, the FreeBSD blocking server, packaging, or when building, releasing, or deploying ferros3.
---

# FerroS3

A single-binary S3-compatible proxy that serves local directories as buckets. No database,
no multipart, no object metadata store — the filesystem *is* the truth, and an object's
key is its path under the bucket's storage root.

Two things shape almost every decision in this codebase:

1. **It ships to FreeBSD 11.2.** That target has no `getrandom(2)`, a 32-bit `d_fileno`,
   and no `pthread_setname_np` — hence a hand-rolled request-id generator, a C shim, a
   patched `std`, and dependencies pinned with `=`. It also does not use `axum::serve`:
   FreeBSD gets a second, blocking thread-per-connection HTTP server in `main.rs`. Do not
   "modernise" any of that without reading
   [references/build-deploy.md](references/build-deploy.md).
2. **The storage mounts are large and sometimes hostile.** Buckets hold hundreds of
   thousands of keys on NFS/ZFS mounts that can hang. Listing is written to touch as few
   directories as possible, and a request timeout exists so a stuck mount fails a request
   instead of eating a connection.

## Commands

| Task | Command |
| --- | --- |
| Build | `cargo build --release` / `make build` |
| Test (38 unit + 14 integration) | `cargo test` / `make test` |
| One test | `cargo test list_objects_pagination_roundtrip` |
| Run locally | `cargo run` (needs `config.yaml` in cwd; debug build also serves `/docs`) |
| Lint | `cargo clippy --all-targets -- -D warnings` (clean; keep it clean) |
| Cross-build Linux | `make build-linux` (musl, needs `cross`) |
| Cross-build FreeBSD 12+ | `make build-freebsd` |
| Cross-build FreeBSD 11.2 | `make build-freebsd11` → `./build-freebsd11.sh` (Docker, slow, ~20 min) |
| Deploy to the 5 real servers | `./my-servers/deploy.sh` (see cautions below) |

CI (`.github/workflows/ci.yml`) runs **only** `make build` and `make test`. Releases fire
on a `v*` tag and build Linux-gnu, FreeBSD 12+, and legacy FreeBSD 11.2 artifacts.

## Code map

| File | What lives there |
| --- | --- |
| [src/lib.rs](../../../src/lib.rs) | Router assembly, layer order, `timeout_middleware`, `build_stamp()`, `load_config`, `build_state` |
| [src/main.rs](../../../src/main.rs) | Two `main`s: tokio/axum everywhere, a blocking thread-per-connection server on FreeBSD |
| [src/blocking_http.rs](../../../src/blocking_http.rs) | Platform-independent HTTP/1.1 parse+format used by the FreeBSD server — kept out of the `cfg` gate so it compiles and unit-tests everywhere |
| [src/auth/mod.rs](../../../src/auth/mod.rs) | `auth_middleware`: SigV4 header auth, SigV4 presigned-query auth, HTTP Basic, query parsing |
| [src/auth/sigv4.rs](../../../src/auth/sigv4.rs) | Canonical request, string-to-sign, signing key, constant-time compare |
| [src/handlers/object.rs](../../../src/handlers/object.rs) | GET/HEAD/PUT/DELETE object, ranges, CopyObject, `?acl`, `safe_join` |
| [src/handlers/list.rs](../../../src/handlers/list.rs) | ListObjects v1/v2: the ordered, prefix-scoped, early-exit walk (the most intricate file here) |
| [src/handlers/bucket.rs](../../../src/handlers/bucket.rs) | ListBuckets, HeadBucket |
| [src/handlers/admin.rs](../../../src/handlers/admin.rs) | `POST /_admin/presign` — mints presigned URLs |
| [src/error.rs](../../../src/error.rs) | `S3ErrorType` → S3 XML error bodies, `/dev/urandom` request ids |
| [src/config.rs](../../../src/config.rs) | `Config` + serde defaults |
| [src/state.rs](../../../src/state.rs), [src/cache.rs](../../../src/cache.rs) | `AppState`, bounded stat cache |
| [src/openapi.rs](../../../src/openapi.rs) | utoipa doc structs; `/docs` + `/openapi.json`, **debug builds only** |
| [build.rs](../../../build.rs) | Git/version stamping, FreeBSD C shim compilation |

## Request lifecycle

Startup, routing, middleware order, the two servers, the stat cache and the error model
are written up in [references/architecture.md](references/architecture.md).

```
socket ─→ timeout_middleware ─→ auth_middleware ─→ route handler ─→ Response
          (outermost, so it also bounds auth)      (returns Response directly, no `?`)
```

Layer order is load-bearing: in axum the **last** `.layer()` added wraps everything before
it, so `timeout_middleware` is registered after `auth_middleware` on purpose. If you add a
layer, decide deliberately whether it belongs inside or outside the timeout.

## Invariants — do not regress these

Each of these exists because it broke something real. Full detail and the failure each one
prevents: [references/invariants.md](references/invariants.md).

- **Path containment.** Every filesystem path comes from `safe_join`, which rejects `..`
  and Windows prefixes. Never build a path by `storage.join(key)`.
- **Atomic PUT.** Write to `.name.pid.n.tmp` in the destination directory → flush →
  optional `fsync` → `rename`. A failed upload must never truncate the live object. Clean
  the temp file up on every error path.
- **Constant-time secret compare.** Signatures and the Basic-auth secret go through
  `constant_time_eq`. The access key is public and may be compared normally.
- **Presign expiry.** `X-Amz-Expires` is mandatory, capped at 7 days, with ±300s skew.
- **`?acl` is not an object write.** `PUT ...?acl` must return 200 without touching the
  file; falling through to the object path truncates it.
- **Self-copy is rejected** (`InvalidRequest`) — `fs::copy` would truncate the object.
- **Listing order is key order.** The walk relies on sorting directories as `name/` so
  depth-first traversal equals ascending key order; that is what allows early exit. Any
  change here must keep the reference-listing tests passing.
- **Blocking work is on `spawn_blocking`.** `readdir`/`stat` must never run on an async
  worker.
- **Errors on S3 paths are S3 XML** via `S3ErrorType`, never a bare status or plain text.
- **ETag format is `"{mtime_nanos:x}-{size:x}"`**, produced identically in GET, HEAD, PUT,
  Copy and listing. It is not a content hash — clients compare it across those endpoints.
- **Timeout exemptions**: PUT/POST and response-body streaming are deliberately unbounded.
- **Never log the query string** — it carries `X-Amz-Signature` and `X-Amz-Credential`.

## House rules

- **Do not run `cargo fmt` over the repo.** The tree is not rustfmt-clean (~105 hunks
  differ) and CI does not check formatting. A repo-wide format would bury real changes.
  Match the style of the code around your edit.
- **Clippy is clean.** `cargo clippy --all-targets -- -D warnings` passes; keep it that
  way. CI does not run clippy, so nothing catches a new warning but you.
- **Comments explain *why*, and often name the incident.** See `build.rs:15`,
  `packaging/ferros3.rc`, `list.rs:181`. Match that: a comment that restates the code is
  noise here; one that records the failure mode is the point. Keep them at ~90 columns.
- **New dependencies are expensive.** They must build for `x86_64-unknown-freebsd` under
  `-Z build-std` and must not require syscalls newer than FreeBSD 11.2 (this is why
  `error.rs` reads `/dev/urandom` instead of using `uuid`). `tokio` and `axum` are pinned
  with `=` — leave them pinned.
- **Handlers return `Response`** and early-return via `match`, rather than `?` with a
  custom error type. Stay consistent within a file.
- Every behavioral change ships with a test. See [references/testing.md](references/testing.md).

## Workflows

**Changing S3 behavior (handler, headers, status codes)** — read
[references/s3-compat.md](references/s3-compat.md) first for what real clients expect and
what is deliberately unimplemented. Add an integration test in
`tests/filesystem_operations.rs` (it drives a real server over HTTP with `reqwest`), then
update [API.md](../../../API.md), `openapi.yaml`, and the `*_docs` stubs in `src/openapi.rs`.

**Touching ListObjects** — the walk in `collect_entries` is subtle. Read the module docs
on `collect_entries`, `collapsing_group`, and `subtree_may_contain` before editing, and
lean on `test_list_objects_matches_a_reference_listing` and
`test_list_objects_pagination_roundtrip`, which check the walk against a naive reference
implementation. Performance tests (`test_list_objects_stats_only_the_page_it_returns`)
assert the walk does *not* touch what it doesn't return — keep them meaningful.

**Adding a config option** — add the field to `Config` with a `#[serde(default = ...)]`
function so existing deployed `config.yaml` files keep working, extend the
`fsync_defaults_to_true_for_existing_configs` test, then update `config.yaml.example`,
`README.md`, and `API.md`'s configuration table. Note that unknown keys are silently
ignored (a live server config already carries a typo'd `sync: true` that does nothing).

**Releasing / deploying** — [references/build-deploy.md](references/build-deploy.md).
Deployment is `rsync` of a bare binary, so the startup banner from `build_stamp()` is the
only way to tell which revision is running. Never break that line's format.

## Before you call it done

1. `cargo test` passes (38 unit + 14 integration).
2. `cargo clippy --all-targets -- -D warnings` passes.
3. Behavior change → a test that fails without the change.
4. API surface change → `API.md`, `openapi.yaml`, `src/openapi.rs`, `README.md` updated.
5. Config change → `config.yaml.example` + a serde-default test.
6. Anything touching `stat`/`readdir`/threads/randomness → re-read the FreeBSD 11 notes in
   [references/build-deploy.md](references/build-deploy.md) before assuming it is portable.

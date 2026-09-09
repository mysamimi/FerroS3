# Testing

`cargo test` runs 38 unit tests (in-module `#[cfg(test)]`) and 14 integration tests, in
about 2 seconds. There is no fixture server to start and nothing to clean up by hand.

## Where a test belongs

| Kind | Location | Style |
| --- | --- | --- |
| Pure logic (range parsing, `safe_join`, prefix pruning, HTTP framing) | `#[cfg(test)] mod tests` in the same file | plain `#[test]` |
| Handler behavior against a real filesystem | `#[cfg(test)] mod tests` in `handlers/list.rs` | `#[tokio::test]`, call the handler function directly, assert on the XML |
| End-to-end over HTTP (status codes, headers, auth, client compatibility) | `tests/filesystem_operations.rs` | `#[tokio::test]` driving `TestServer` with `reqwest` |
| Middleware timing | `lib.rs` tests | `#[tokio::test(start_paused = true)]` |

## The integration harness

`TestServer::start()` in `tests/filesystem_operations.rs` binds `127.0.0.1:0`, builds real
`Config`/`AppState`, spawns `axum::serve`, and points a bucket at a `TempDir`. It exposes
`write`, `read`, `exists`, `head_etag`, `list`, `copy`, `rename`, `delete`, `read_range`,
`get_object_acl`, `content_md5`, `presign_url`. Auth is a hard-coded Basic header
(`test_key:test_secret`). `Drop` aborts the server task, and the `TempDir`s clean
themselves — add fields to the struct, never leave files behind.

Prefer extending this harness over hand-rolling a client: a test that goes through HTTP
catches the header/status/XML regressions that calling a handler directly does not.

## Time-dependent tests

Use `#[tokio::test(start_paused = true)]` and let tokio's virtual clock skip the wait — see
`slow_request_times_out_instead_of_hanging`. `tokio`'s `test-util` feature is a dev
dependency for exactly this. Never `sleep` in real time in a test.

## Listing tests

`handlers/list.rs` carries the heaviest suite, and two of its tests are the safety net for
any change to the walk:

- `test_list_objects_matches_a_reference_listing` — builds a tree, computes the expected
  page with a naive "collect everything, sort, slice" reference implementation
  (`reference_keys` / `reference_page`), and asserts the optimised walk agrees.
- `test_list_objects_pagination_roundtrip` — pages through with `max-keys` and asserts the
  concatenation equals the single-shot listing, with no duplicates or gaps.

If you change `collect_entries`, `read_children`, `collapsing_group`,
`subtree_may_contain`, or `prefix_search_root`, extend the reference test's tree to cover
the new case rather than only asserting the new behavior in isolation.

`test_list_objects_stats_only_the_page_it_returns` is a *performance* assertion — it exists
to catch a change that quietly reintroduces a full traversal. Keep it honest.

Each listing test uses its own `./test_list_data_*` directory relative to the cwd (so
`cargo test` must run from the repo root) and removes it at the start and end. When adding
one, pick a fresh unique directory name; sharing one makes the tests race under parallel
execution.

## Writing a regression test

State the failure in the test name — the suite reads as a list of things that once went
wrong: `put_object_acl_does_not_truncate_the_object`,
`malformed_sigv4_date_is_rejected_not_panicking`,
`copy_object_onto_itself_is_rejected_and_preserves_content`. Assert the *consequence*
(the object still has its content) rather than only the status code.

## What is not covered

- The FreeBSD blocking server's socket glue in `main.rs` — only the pure logic in
  `blocking_http.rs` is unit-tested. Anything touching the accept loop, keep-alive
  handling, or `100-continue` must be verified on a real FreeBSD host.
- SigV4 *header* auth end-to-end against a real AWS SDK. `aws s3` / `aws s3api` against a
  locally running server is the manual check; the automated suite covers Basic auth and
  presigned-query auth only.
- Concurrency (parallel PUTs to one key, listing during writes). The invariants are
  designed for it; nothing asserts it.

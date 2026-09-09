# Invariants

Each entry is a rule, the failure it prevents, and where it is enforced. Treat a change
that breaks one as a bug even if the tests still pass — then add the missing test.

## Security

**Path containment.** `safe_join(storage, key)` (`handlers/object.rs`) walks the key's
components and returns `None` on `ParentDir` or a Windows `Prefix`; `RootDir` and `CurDir`
are skipped. Every object handler routes its path through it, and listing has the parallel
`prefix_search_root`. Without it, `GET /bucket/../../etc/passwd` reads outside the bucket.
Tested by `test_safe_join`.

**Constant-time comparison of secrets.** `constant_time_eq` (`auth/sigv4.rs`) with a
`black_box` on the accumulated difference, used for the computed signature and for the
Basic-auth secret. The *access key* is public and is compared with `==` on purpose. A
short-circuiting compare leaks how many leading bytes of a guess were right.

**Presigned URLs expire.** `verify_query_auth` requires `X-Amz-Expires` in `1..=604800`
(AWS's 7-day cap), parses `X-Amz-Date` as `%Y%m%dT%H%M%SZ`, and rejects `age > expires` or
`age < -300` (bounded future skew). Without this a presigned URL is a permanent bearer
credential. Tested by `presigned_url_is_rejected_after_it_expires`.

**Attacker-controlled dates never index blindly.** `verify_signature` checks
`date.len() >= 8 && date.is_char_boundary(8)` before slicing `date[..8]`. A short or
multi-byte `x-amz-date` used to panic the worker. Tested by
`malformed_sigv4_date_is_rejected_not_panicking`.

**Query strings stay out of logs.** `auth_middleware` logs `req.uri().path()`, never the
full URI: the query carries `X-Amz-Signature` and `X-Amz-Credential`.

**Auth is fail-closed.** If `config.auth` is set and no recognised credential is present,
the request is denied. If `auth` is absent, everything is public — that is the documented
behavior, not an oversight. A bare access key without a signature must not authenticate
(`bare_access_key_is_rejected_but_valid_basic_auth_passes`).

**Query parsing splits on the first `=` only.** `parse_query` keeps valueless flags (`acl`)
and values containing `=` (base64 tokens). The old `split('=')`-with-a-length-guard dropped
both, which corrupted the SigV4 canonical query string for those requests.

**Pre-auth resource bounds.** On FreeBSD, `MAX_CONNECTIONS` caps connection threads and
`MAX_BODY_BYTES` caps a buffered body — both are enforced before auth runs, deliberately.

## Data integrity

**PUT is atomic.** Write to `temp_path_for(path)` — `.{name}.{pid}.{counter}.tmp` in the
*same directory*, so the rename stays on one filesystem — then `flush()`, then `sync_all()`
if `config.fsync`, then `fs::rename` over the target. Every failure path removes the temp
file. The live object survives an aborted upload, and concurrent PUTs to one key cannot
interleave. Tested by `put_object_returns_matching_etag_and_leaves_no_temp_files` and
`put_object_overwrite_fully_replaces_content`.

**`flush()` before claiming success.** ENOSPC/EIO can surface only at close; swallowing it
by dropping the file would ack a lost write.

**`PUT ?acl` is a no-op, not a write.** `has_acl_query` short-circuits before the object
path. Otherwise `File::create` truncates the object and overwrites it with the ACL XML.
Tested by `put_object_acl_does_not_truncate_the_object`.

**Self-copy is refused.** `copy_object` canonicalises both sides and returns
`InvalidRequest` when they match, because `fs::copy` opens the destination `O_TRUNC` before
reading the source — a self-copy would zero the object. S3 refuses it too. Tested by
`copy_object_onto_itself_is_rejected_and_preserves_content`.

**Copy results describe the destination.** ETag/LastModified come from the destination's
post-copy metadata so a following HEAD/GET agrees. Tested by
`copy_object_result_etag_matches_destination_head`.

**A file that vanishes mid-listing is skipped, not fatal.** `collect_entries` uses
`symlink_metadata` and `continue`s on error — a concurrent DELETE between readdir and stat
must not kill the listing. Symlinks are reported as objects, never followed.

## Protocol compatibility

**ETag format**: `"{mtime_nanos:x}-{size:x}"`, quoted, generated identically in
`get_object`, `head_object`, `put_object`, `copy_object` and `collect_entries`. It is *not*
a content hash; clients that verify content compute their own MD5 over the body
(`md5_is_computed_from_object_body_not_etag`). Change the format in one place only if you
change it in all five.

**Dates**: `Last-Modified` uses the IMF-fixdate `http_date()` helper — `to_rfc2822()` emits
`+0000` instead of `GMT` and strict clients reject it. XML `LastModified` uses RFC 3339.
Tested by `responses_use_http_date_and_advertise_accept_ranges`.

**`Accept-Ranges: bytes`** is advertised on GET (full and partial) and HEAD.

**XML nesting**: `<Buckets>` wraps repeated `<Bucket>` elements. Serialising the vector
directly produced repeated bare `<Buckets>` and every SDK saw zero buckets. Tested by
`list_buckets_nests_bucket_elements`.

**Errors are S3 XML** with `Code`/`Message`/`Resource`/`RequestId` — never plain text on an
S3 route. `DELETE` of a missing key returns 204, as S3 does.

## Performance / operations

**Listing order equals key order.** `read_children` sorts by the key a child contributes —
a directory sorts as `name/`, and `'.' (0x2E) < '/' (0x2F)`, so `a.txt` < `a/…` < `ab.txt`.
Depth-first traversal therefore emits ascending keys, which is exactly what lets the walk
stop at `max_keys + 1` items instead of enumerating and sorting the whole subtree. Break
the sort key and pagination silently returns wrong pages.

**The walk prunes before it reads.** `subtree_may_contain` (prefix / start_after overlap),
`collapsing_group` + `subtree_has_file` (a `/`-delimited directory collapses to one
CommonPrefix, proven non-empty by an unsorted probe that stops at the first file), and
`settled_group` (skip the rest of a group already emitted or ruled out). The explicit stack
exists *because* `walkdir` reads and sorts a directory the moment it hands it to you — the
saving comes from not opening it at all. `test_list_objects_stats_only_the_page_it_returns`
asserts this.

**Blocking syscalls run on `spawn_blocking`.** `readdir`/`stat` on an async worker stalls
the whole runtime for the duration of the traversal.

**Streaming reads use a 64 KiB buffer.** `ReaderStream::with_capacity(.., STREAM_BUF_SIZE)`
— the 4 KiB default costs 16× the syscalls and wakeups on large downloads.

**The stat cache is bounded** by `cache_size` and evicts; it must never be replaced with an
unbounded map.

**The startup banner is an operational contract.** `build_stamp()` prints
`ferros3 <ver> | version <describe> | commit <sha> | built <ts>` as the first log line;
`my-servers/deploy.sh` greps for it to confirm the new binary is the one running. Deploys
are bare rsynced binaries with no package version to read back.

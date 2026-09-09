# S3 compatibility surface

Authoritative human reference: [API.md](../../../../API.md). Machine reference:
`openapi.yaml` (static) and `/openapi.json` + `/docs` on a **debug** build, behind HTTP
Basic (`access_key` / `secret_key`).

## Implemented

| Operation | Route | Notes |
| --- | --- | --- |
| ListBuckets | `GET /` | buckets come from config; `CreationDate` is a constant |
| HeadBucket | `HEAD /:bucket[/]` | 200 or `NoSuchBucket` |
| ListObjects v1/v2 | `GET /:bucket[/]` | `prefix`, `delimiter`, `max-keys`, `marker`, `continuation-token`, `list-type=2` |
| GetObject | `GET /:bucket/*key` | `Range` (incl. suffix ranges), `?acl` probe |
| HeadObject | `HEAD /:bucket/*key` | served from the stat cache when warm |
| PutObject | `PUT /:bucket/*key` | atomic; returns `ETag` |
| CopyObject | `PUT` + `x-amz-copy-source` | returns `<CopyObjectResult>` |
| PutObjectAcl | `PUT ...?acl` | accepted, **no-op**, returns 200 |
| DeleteObject | `DELETE /:bucket/*key` | always 204, even for a missing key |
| Presign helper | `POST /_admin/presign` | non-standard; requires auth; returns a URL as `text` |

## Deliberately not implemented

Bucket create/delete, **multipart upload**, DeleteObjects (bulk), object tagging, real ACL
storage, lifecycle, replication, notifications, policies, versioning, server-side
encryption, user-defined `x-amz-meta-*` metadata, and `Content-Type` sniffing — every GET
answers `application/octet-stream`; adding real content types means a new dependency and
storing or guessing a type per key.

Before adding one, weigh it against the deployment reality: FreeBSD 11 hosts, no database,
and a filesystem that has to remain readable by other tools. Multipart in particular would
need a part store and a manifest, which is a genuine design change, not a handler.

## Client-visible details that bite

- **Path-style only.** Virtual-host-style (`bucket.host/key`) is not routed. Clients must
  be configured with `force_path_style` / `--endpoint-url`.
- **`ETag` is `"{mtime_nanos:x}-{size:x}"`, not an MD5.** Clients or tests that verify
  content must hash the body themselves.
- **`ListObjects` pagination tokens are the last emitted key**, not an opaque cursor, and
  they are used as an *exclusive* start for both `marker` (v1) and `continuation-token`
  (v2). `NextMarker`/`NextContinuationToken` are only emitted when truncated.
- **A key ending in `/` is not a directory marker.** Directories exist implicitly; empty
  directories are excluded from listings entirely (`subtree_has_file`).
- **Symlinks are listed as objects** and are not followed by the listing walk.
- **Delimiters other than `/` work**, including a delimiter that falls inside a filename
  (`a-1.txt` collapsing at `-`) — that path is separate from the fast `/`-collapse.
- **`prefix` without a trailing slash spans the boundary**: `logs/2024` matches both
  `logs/2024.txt` and `logs/2024/jan.txt`, as S3 does.
- **HTTP `Last-Modified` is IMF-fixdate; XML `LastModified` is RFC 3339.** Two formats, on
  purpose.
- **Auth accepts three shapes**: SigV4 `Authorization` header, SigV4 presigned query
  parameters, and HTTP Basic (`access_key:secret_key`) — the last is a convenience for
  curl and the Swagger UI, not something AWS clients use.
- **With no `auth:` block in the config, everything is unauthenticated.**
- **Region and service in a signature are taken from the client's credential scope**, not
  validated against a configured region. `/_admin/presign` mints URLs for `us-east-1`.

## Manual check with the AWS CLI

```bash
aws --endpoint-url http://127.0.0.1:8080 s3 ls
aws --endpoint-url http://127.0.0.1:8080 s3 cp ./file.txt s3://my-bucket/path/file.txt
aws --endpoint-url http://127.0.0.1:8080 s3api list-objects-v2 --bucket my-bucket --prefix logs/ --delimiter /
```

with credentials from `config.yaml`. This is the only end-to-end check of SigV4 *header*
auth — the automated suite covers Basic and presigned-query auth.

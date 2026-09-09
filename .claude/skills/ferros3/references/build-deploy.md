# Build, cross-compile, package, deploy

## Targets

| Target | How | Notes |
| --- | --- | --- |
| Host (dev) | `cargo build --release` | debug builds additionally serve `/docs` |
| `x86_64-unknown-linux-musl` | `make build-linux` | needs `cross`; this is what the Linux servers run |
| `x86_64-unknown-freebsd` (12+) | `make build-freebsd` | needs `cross` |
| FreeBSD **11.2** | `make build-freebsd11` / `./build-freebsd11.sh` | Docker + nightly + `-Z build-std`; slow |
| Docker image | `make docker-build` | `Dockerfile`, rust:1.75-slim → debian-slim |

Release builds use `lto = "thin"` and `codegen-units = 1`: the hot paths (axum routing,
tokio I/O, sha2/hmac) span many small crates, so cross-crate inlining is worth the build
time.

## The build stamp

`build.rs` bakes `FERROS3_GIT_COMMIT`, `FERROS3_GIT_DESCRIBE` and `FERROS3_BUILD_EPOCH`
into the binary; `build_stamp()` prints them as the first log line. Rules:

- Both git values can be supplied via environment variables — do that when building in a
  container that only has the sources.
- A missing or refusing `git` (containers hit "dubious ownership" on bind-mounted
  checkouts, hence `-c safe.directory=*`) must **never fail the build**; the values fall
  back to `unknown`.
- The script re-stamps on `.git/HEAD` and `.git/refs` changes.

## FreeBSD 11.2 — why it is this complicated

Rust dropped FreeBSD 11 from its baseline, and FreeBSD 12.0 widened `ino_t` to 64 bits
("ino64"), changing the layout of `struct stat` and `struct dirent`. A stock cross build
links a `std`/`libc` compiled for the 12+ ABI, so on an 11.x host every `stat` and
`readdir` reads fields at the wrong offsets: directory listings come back with empty names
and duplicate entries, file sizes and mtimes are garbage. It *runs* — it is just wrong.

`Dockerfile.freebsd11` therefore:

1. downloads the FreeBSD 11.2 `base.txz` as a sysroot,
2. patches `std`'s `readdir` to cast the 32-bit `d_fileno` (the `grep`s around the `sed` are
   deliberate — a future nightly that rewrites that code must fail the build loudly rather
   than silently produce a broken binary),
3. sets `--cfg libc_unstable_freebsd_version="11"` plus the legacy
   `RUST_LIBC_UNSTABLE_FREEBSD_VERSION=11` env var (old and new `libc` read different
   ones), and links with clang/lld against the sysroot,
4. builds with `-Z build-std` — a prebuilt `std` from rustup carries the wrong ABI and
   cannot be fixed after the fact.

`src/freebsd11_shim.c` (compiled by `build.rs` only for freebsd targets) provides
`pthread_setname_np`, which FreeBSD gained in 12.2.

`build-freebsd11.sh` probes Debian mirrors for speed/freshness/architecture before building
the image, because `deb.debian.org` is unusably slow on some networks and a partial mirror
will pass a naive reachability check and then 404 on `binary-arm64/Packages`. Set
`DEBIAN_MIRROR` / `DEBIAN_SECURITY_MIRROR` to skip a probe.

The container must resolve dependencies itself rather than inherit the host's
`Cargo.lock`, and the checkout is bind-mounted at `/app` — so the script stashes the lock
on the host, runs the build against a clean tree, and restores the original from an `EXIT`
trap whether the build succeeded or not. Do not move that deletion back inside the
container: it would delete the developer's own lock and write a root-owned replacement
into the checkout. `target/` is still written as root by the container, which is why the
release workflow chowns it afterwards.

**Consequences for code changes:** anything touching randomness, thread names, `dirent`,
`stat`, or an async socket path is FreeBSD-11-sensitive. This is why `error.rs` reads
`/dev/urandom` instead of using `uuid`/`getrandom`, why there is a blocking HTTP server,
and why `tokio = "=1.35.1"` and `axum = "=0.7.5"` are pinned with `=`.

## Packaging and service units

Both units in `packaging/` assume the binary and its `config.yaml` sit together in `/app`,
because the config path is relative.

- `ferros3.service` (systemd): `WorkingDirectory=/app` is load-bearing; without it the
  server starts in `/` and dies on a missing config. Waits on `network-online.target`
  because it binds a specific address.
- `ferros3.rc` (FreeBSD rc.d): runs the server under `daemon(8)` with `-r` (restart),
  `-S -T ferros3` (stdout/stderr → syslog; syslog rather than a file because `newsyslog`
  rotates by HUPing syslogd, whereas `daemon -o` would keep writing into the rotated-away
  inode), `-f` (detach the supervisor's own descriptors, or
  `ssh host 'service ferros3 start'` never returns), and two pidfiles: `-P` for the
  supervisor that rc signals, `-p` for the server itself. Overrides: `ferros3_dir`,
  `ferros3_user`.

## Deploying (`my-servers/`)

`my-servers/` is **gitignored** and holds real production configuration: five hosts
(`everest`, `fuji`, `karkas`, `kilimanjaro`, `sabalan` — SSH aliases) with real access and
secret keys and real storage mount paths. Never commit it, never echo a key from it into
output, and never paste one into an artifact, a commit message, or an external service.

`./my-servers/deploy.sh` loops over every `my-servers/*.yaml`, and for each host:
`rsync` the binary to `/app/ferros3` and the yaml to `/app/config.yaml`, install the
service unit, restart it, then verify by grepping the startup banner out of the logs
(`/var/log/daemon.log` on the TrueNAS FreeBSD host, `journalctl` on the Linux hosts). It
picks the FreeBSD 11 binary for `everest` and the musl binary for everyone else, and it
expects both to be built already.

This restarts production services on five machines. **Ask before running it**, and never
run it as a way of "testing" a change. `./my-servers/check.sh` is the harmless one — it
curls each host's port 8088.

The `verify_service` greps end in `|| true` on purpose: without it, a banner missing from
the log trips `set -e` and aborts the deploy before the remaining servers are updated.

Note that the deployed configs are not perfectly in sync with the schema — `fuji.yaml`
carries `sync: true`, which is not a config key (the field is `fsync`) and is silently
ignored by serde. Unknown keys never fail to parse; that cuts both ways.

## Releasing

Tag `v*` and push. `.github/workflows/release.yml` builds `x86_64-unknown-linux-gnu` and
`x86_64-unknown-freebsd` with `cross`, plus the legacy FreeBSD 11.2 artifact through the
Docker pipeline, tars each binary and attaches them to the GitHub release.

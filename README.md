# 🦀 FerroS3

[![CI](https://github.com/mysamimi/ferros3/actions/workflows/ci.yml/badge.svg)](https://github.com/mysamimi/ferros3/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![GitHub release](https://img.shields.io/github/v/release/mysamimi/ferros3)](https://github.com/mysamimi/ferros3/releases)
[![Rust](https://img.shields.io/badge/Rust-2021-orange.svg?logo=rust)](https://www.rust-lang.org)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20FreeBSD-blue)](#-cross-compilation-freebsdlinux)

![FerroS3 Banner](assets/banner.png)

**FerroS3** is a high-performance, minimalist S3-compatible proxy written in Rust. It serves local filesystem directories as S3 buckets, providing a lightweight alternative to MinIO for resource-constrained environments like old FreeBSD kernels or embedded Linux.

---

## 🚀 Features

-   **High Performance**: Built on top of `Tokio` and `Axum` for asynchronous I/O.
-   **Full AWS SigV4 Support**: Compatible with official AWS CLI, SDKs, and standard S3 clients.
-   **Modular Architecture**: Clean, extensible code structure.
-   **In-Memory Stat Cache**: Lightning-fast metadata retrieval using `DashMap`.
-   **Advanced Listing**: Supports ListObjects v1/v2 with `prefix` and `delimiter`.
-   **Streaming Support**: Handles large file uploads and downloads (Range Requests) efficiently.
-   **Cross-Platform**: Designed to run seamlessly on Linux, macOS, and **FreeBSD**.
-   **Zero External DB**: Pure filesystem-backed storage.
-   **Operational Safeguards**: Configurable request timeout (a hung storage mount fails the request instead of holding the connection) and configurable PUT `fsync` durability.
-   **Build Stamp**: Every binary knows the revision it was built from and prints it on startup.

## 📦 Installation

### Pre-built Binaries
Check the [Releases](https://github.com/mysamimi/ferros3/releases) page for pre-built binaries for Linux and FreeBSD.

### Build from Source
```bash
git clone https://github.com/mysamimi/ferros3.git
cd ferros3
cargo build --release
```

The build stamps the git revision and build time into the binary, and the server prints
them as the first line of its log:

```
ferros3 0.1.0 | version v0.1.0-3-gabc1234 | commit abc123456789 | built 2026-09-06T12:00:00Z
```

When building outside a git checkout (for example inside a container that only has the
sources), set `FERROS3_GIT_COMMIT` and `FERROS3_GIT_DESCRIBE` to fill in those values;
otherwise they read `unknown`. A missing `git` never fails the build.

## 🛠️ Configuration

Copy `config.yaml.example` to `config.yaml` and adjust the values:

```bash
cp config.yaml.example config.yaml
```

Example configuration:

```yaml
port: 8080
endpoint: "0.0.0.0"
verbose: true
# Maximum number of entries in the object stat cache (older entries are evicted).
cache_size: 10000
# Fsync each uploaded object before acknowledging the PUT (default true). Set to false
# to trade crash durability for PUT latency when this proxy is not the source of truth.
fsync: true
# Give up on a request that hasn't produced a response within this many seconds, so a
# hung storage mount fails the request instead of holding the connection open forever.
# Set to 0 to disable. Uploads (PUT/POST) and response-body streaming are not bounded.
request_timeout_secs: 30
auth:
  access_key: "YOUR_ACCESS_KEY"
  secret_key: "YOUR_SECRET_KEY"
buckets:
  - name: "my-bucket"
    storage: "/path/to/local/data"
```

`config.yaml` is read as a **relative path**, so the server must be started from the
directory that contains it. See [API.md](./API.md) for the full option reference.

## 📚 API Documentation

- Available only in non-production builds (`cargo run` / debug builds).
- Live Swagger UI: `http://127.0.0.1:8080/docs`
- Live OpenAPI JSON: `http://127.0.0.1:8080/openapi.json`
- Human-readable API reference: [API.md](./API.md)
- Static OpenAPI file: [openapi.yaml](./openapi.yaml)

Swagger UI uses HTTP Basic auth:
- Username: `access_key`
- Password: `secret_key`

## 🏗️ Cross-Compilation (FreeBSD/Linux)

### Modern Targets (Linux & FreeBSD 12+)
To cross-compile for modern FreeBSD or Linux from a macOS/Windows host:

1.  Install `cross`:
    ```bash
    cargo install cross --git https://github.com/cross-rs/cross.git
    ```
2.  Build for your target:
    ```bash
    # For FreeBSD 12+
    make build-freebsd
    
    # For Linux (x86_64)
    make build-linux
    ```

### Legacy Targets (FreeBSD 11.2)
If you need to deploy FerroS3 to an older system (like FreeBSD 11.2 or older TrueNAS Core versions), standard cross-compilation will fail due to `libc` version mismatches. 

We provide a dedicated Docker-based build pipeline and a small FreeBSD 11 compatibility shim for this target. Please see the [Legacy FreeBSD Build Guide](legacy-freebsd-build-osx.md) for detailed instructions.

```bash
make build-freebsd11   # or: ./build-freebsd11.sh
```

`build-freebsd11.sh` probes a list of Debian mirrors and builds the image against the
fastest reachable, up-to-date one (`deb.debian.org` is unusably slow on some networks).
Set `DEBIAN_MIRROR` and/or `DEBIAN_SECURITY_MIRROR` to skip the corresponding probe:

```bash
DEBIAN_MIRROR=https://deb.debian.org/debian ./build-freebsd11.sh
```

## 🐳 Docker

```bash
docker build -t ferros3 .
docker run -p 8080:8080 -v ./config.yaml:/app/config.yaml -v ./data:/data ferros3
```

## 🔧 Running as a Service

Ready-to-use service units live in [packaging/](packaging/). Both assume the binary and
its `config.yaml` sit together in `/app`, because the config path is relative.

### Linux (systemd)

```bash
sudo install -D -m 755 target/release/ferros3 /app/ferros3
sudo cp config.yaml /app/config.yaml
sudo cp packaging/ferros3.service /etc/systemd/system/ferros3.service
sudo systemctl daemon-reload
sudo systemctl enable --now ferros3
journalctl -u ferros3 -f
```

### FreeBSD (rc.d)

```sh
install -m 755 target/x86_64-unknown-freebsd/release/ferros3 /app/ferros3
cp config.yaml /app/config.yaml
cp packaging/ferros3.rc /usr/local/etc/rc.d/ferros3
chmod 755 /usr/local/etc/rc.d/ferros3
sysrc ferros3_enable=YES
service ferros3 start
```

The rc script runs the server under `daemon(8)`, which restarts it if it exits and
forwards its output to syslog under the `ferros3` tag. Optional `rc.conf` overrides:
`ferros3_dir` (default `/app`) and `ferros3_user` (default `root`).

## 🧪 Testing

Run the test suite (including filesystem integration tests) with:

```bash
cargo test
# or
make test
```

## 🤝 Contributing

Contributions are welcome! Whether it's a bug fix, a new feature, or improved documentation, we appreciate your help.

### Getting Started

1.  **Fork** the repository and clone your fork.
2.  Create a feature branch:
    ```bash
    git checkout -b feature/my-awesome-feature
    ```
3.  Make your changes.

### Before Submitting

Please make sure your changes pass the following checks:

```bash
cargo fmt --all          # Format the code
cargo clippy -- -D warnings   # Lint (treat warnings as errors)
cargo test               # Run the test suite
```

### Submitting a Pull Request

1.  Commit your changes with a clear, descriptive message.
2.  Push to your fork and open a Pull Request against the `main` branch.
3.  Describe **what** you changed and **why**. Link any related issues.

### Reporting Issues

Found a bug or have a feature request? Please [open an issue](https://github.com/mysamimi/ferros3/issues) with:

- A clear description of the problem or request.
- Steps to reproduce (for bugs).
- Your platform (Linux / macOS / FreeBSD) and FerroS3 version.

## 📝 License
This project is licensed under the MIT License.

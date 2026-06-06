# 🦀 FerroS3

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

## 📦 Installation

### Pre-built Binaries
Check the [Releases](https://github.com/mysamimi/ferros3/releases) page for pre-built binaries for Linux and FreeBSD.

### Build from Source
```bash
git clone https://github.com/mysamimi/ferros3.git
cd ferros3
cargo build --release
```

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
cache_size: 10000
auth:
  access_key: "YOUR_ACCESS_KEY"
  secret_key: "YOUR_SECRET_KEY"
buckets:
  - name: "my-bucket"
    storage: "/path/to/local/data"
```

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

## 🐳 Docker

```bash
docker build -t ferros3 .
docker run -p 8080:8080 -v ./config.yaml:/app/config.yaml -v ./data:/data ferros3
```

## 📝 License
This project is licensed under the MIT License.

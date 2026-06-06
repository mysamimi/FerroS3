use ferros3::{
    build_app,
    build_state,
    config::{AuthConfig, BucketConfig, Config},
};
use md5::{Digest, Md5};
use reqwest::{Client, StatusCode};
use tempfile::TempDir;
use tokio::{fs, net::TcpListener, task::JoinHandle};

struct TestServer {
    _storage_dir: TempDir,
    _source_dir: TempDir,
    bucket: String,
    base_url: String,
    auth_header: String,
    client: Client,
    handle: JoinHandle<()>,
}

impl TestServer {
    async fn start() -> Self {
        let storage_dir = TempDir::new().unwrap();
        let source_dir = TempDir::new().unwrap();
        let bucket = "test-bucket".to_string();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let config = Config {
            port: address.port(),
            endpoint: "127.0.0.1".to_string(),
            verbose: false,
            cache_size: 32,
            auth: Some(AuthConfig {
                access_key: "test_key".to_string(),
                secret_key: "test_secret".to_string(),
            }),
            buckets: vec![BucketConfig {
                name: bucket.clone(),
                storage: storage_dir.path().display().to_string(),
            }],
        };

        let app = build_app(build_state(&config));
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Self {
            _storage_dir: storage_dir,
            _source_dir: source_dir,
            bucket,
            base_url: format!("http://{}", address),
            auth_header: "test_key".to_string(),
            client: Client::new(),
            handle,
        }
    }

    fn object_url(&self, key: &str) -> String {
        format!(
            "{}/{}/{}",
            self.base_url,
            self.bucket,
            urlencoding::encode(key)
        )
    }

    async fn write(&self, key: &str, source_path: &std::path::Path) {
        let payload = fs::read(source_path).await.unwrap();
        let response = self
            .client
            .put(self.object_url(key))
            .header("Authorization", &self.auth_header)
            .body(payload)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    async fn read(&self, key: &str) -> Vec<u8> {
        let response = self
            .client
            .get(self.object_url(key))
            .header("Authorization", &self.auth_header)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.bytes().await.unwrap().to_vec()
    }

    async fn exists(&self, key: &str) -> bool {
        self.client
            .head(self.object_url(key))
            .header("Authorization", &self.auth_header)
            .send()
            .await
            .unwrap()
            .status()
            == StatusCode::OK
    }

    async fn list(&self, prefix: &str, delimiter: Option<&str>) -> String {
        let mut request = self
            .client
            .get(format!("{}/{}/", self.base_url, self.bucket))
            .header("Authorization", &self.auth_header)
            .query(&[("prefix", prefix)]);

        if let Some(delimiter) = delimiter {
            request = request.query(&[("delimiter", delimiter)]);
        }

        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.text().await.unwrap()
    }

    async fn directory_exists(&self, prefix: &str) -> bool {
        let listing = self.list(prefix, Some("/")).await;
        listing.contains(&format!("<Key>{}", prefix))
            || listing
                .matches(&format!("<Prefix>{}</Prefix>", prefix))
                .count()
                > 1
    }

    async fn md5(&self, key: &str) -> String {
        let mut hasher = Md5::new();
        hasher.update(self.read(key).await);
        format!("{:x}", hasher.finalize())
    }

    async fn copy(&self, source_key: &str, destination_key: &str) {
        let payload = self.read(source_key).await;
        let response = self
            .client
            .put(self.object_url(destination_key))
            .header("Authorization", &self.auth_header)
            .body(payload)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    async fn rename(&self, source_key: &str, destination_key: &str) {
        self.copy(source_key, destination_key).await;
        self.delete(source_key).await;
    }

    async fn delete(&self, key: &str) {
        let response = self
            .client
            .delete(self.object_url(key))
            .header("Authorization", &self.auth_header)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    async fn read_range(&self, key: &str, range: &str) -> Vec<u8> {
        let response = self
            .client
            .get(self.object_url(key))
            .header("Authorization", &self.auth_header)
            .header("Range", range)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        response.bytes().await.unwrap().to_vec()
    }

    async fn presign_url(&self, key: &str, method: &str) -> String {
        let response = self
            .client
            .post(format!("{}/_admin/presign", self.base_url))
            .header("Authorization", &self.auth_header)
            .query(&[
                ("bucket", self.bucket.as_str()),
                ("key", key),
                ("method", method),
                ("expires", "60"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.text().await.unwrap()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[tokio::test]
async fn filesystem_operations_work_end_to_end() {
    let server = TestServer::start().await;

    let source_file = server._source_dir.path().join("source.txt");
    let nested_source_file = server._source_dir.path().join("nested.txt");

    fs::write(&source_file, b"hello ferros3").await.unwrap();
    fs::write(&nested_source_file, b"nested payload").await.unwrap();

    server.write("docs/source.txt", &source_file).await;
    server.write("docs/archive/nested.txt", &nested_source_file).await;

    assert!(server.exists("docs/source.txt").await);
    assert!(server.directory_exists("docs/").await);

    let payload = server.read("docs/source.txt").await;
    assert_eq!(payload, b"hello ferros3");

    let listing = server.list("docs/", Some("/")).await;
    assert!(listing.contains("<Key>docs/source.txt</Key>"));
    assert!(listing.contains("<Prefix>docs/archive/</Prefix>"));

    let checksum = server.md5("docs/source.txt").await;
    assert_eq!(checksum, "1922cd4fcab920a5f64e3426a834deba");

    server.copy("docs/source.txt", "docs/copied.txt").await;
    assert_eq!(server.read("docs/copied.txt").await, b"hello ferros3");

    server.rename("docs/copied.txt", "docs/renamed.txt").await;
    assert!(!server.exists("docs/copied.txt").await);
    assert!(server.exists("docs/renamed.txt").await);

    let range_payload = server.read_range("docs/renamed.txt", "bytes=0-4").await;
    assert_eq!(range_payload, b"hello");

    let presigned_url = server.presign_url("docs/renamed.txt", "GET").await;
    let presigned_response = server.client.get(presigned_url).send().await.unwrap();
    assert_eq!(presigned_response.status(), StatusCode::OK);
    assert_eq!(presigned_response.bytes().await.unwrap().as_ref(), b"hello ferros3");

    server.delete("docs/renamed.txt").await;
    server.delete("docs/archive/nested.txt").await;
    server.delete("docs/missing.txt").await;

    assert!(!server.exists("docs/renamed.txt").await);
    assert!(!server.directory_exists("docs/archive/").await);
}

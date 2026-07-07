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
            // Basic base64("test_key:test_secret")
            auth_header: "Basic dGVzdF9rZXk6dGVzdF9zZWNyZXQ=".to_string(),
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

    async fn head_etag(&self, key: &str) -> String {
        let response = self
            .client
            .head(self.object_url(key))
            .header("Authorization", &self.auth_header)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response
            .headers()
            .get("etag")
            .unwrap()
            .to_str()
            .unwrap()
            .trim_matches('"')
            .to_string()
    }

    async fn get_object_acl(&self, key: &str) -> String {
        let response = self
            .client
            .get(format!("{}?acl", self.object_url(key)))
            .header("Authorization", &self.auth_header)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.text().await.unwrap()
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

    async fn content_md5(&self, key: &str) -> String {
        let mut hasher = Md5::new();
        hasher.update(self.read(key).await);
        format!("{:x}", hasher.finalize())
    }

    async fn copy(&self, source_key: &str, destination_key: &str) {
        let response = self
            .client
            .put(self.object_url(destination_key))
            .header("Authorization", &self.auth_header)
            .header(
                "x-amz-copy-source",
                format!("/{}/{}", self.bucket, source_key),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        assert!(body.contains("<CopyObjectResult>"));
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

    let checksum = server.content_md5("docs/source.txt").await;
    let expected_checksum = format!("{:x}", Md5::digest(b"hello ferros3"));
    assert_eq!(checksum, expected_checksum);

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

#[tokio::test]
async fn md5_is_computed_from_object_body_not_etag() {
    let server = TestServer::start().await;
    let source_file = server._source_dir.path().join("payload.bin");
    let source_bytes = b"md5 regression payload";

    fs::write(&source_file, source_bytes).await.unwrap();
    server.write("checksums/payload.bin", &source_file).await;

    let expected_md5 = format!("{:x}", Md5::digest(source_bytes));
    let actual_md5 = server.content_md5("checksums/payload.bin").await;
    let actual_etag = server.head_etag("checksums/payload.bin").await;

    assert_eq!(actual_md5, expected_md5);
    assert_ne!(actual_etag, expected_md5);
    assert!(actual_etag.contains('-'));
}

#[tokio::test]
async fn copy_object_and_acl_probe_are_s3_compatible() {
    let server = TestServer::start().await;
    let source_file = server._source_dir.path().join("copy-source.txt");

    fs::write(&source_file, b"copy me").await.unwrap();
    server.write("copy/source.txt", &source_file).await;

    let acl_xml = server.get_object_acl("copy/source.txt").await;
    assert!(acl_xml.contains("<AccessControlPolicy>"));
    assert!(acl_xml.contains("<Permission>FULL_CONTROL</Permission>"));

    server.copy("copy/source.txt", "copy/target.txt").await;
    assert_eq!(server.read("copy/target.txt").await, b"copy me");
}

#[tokio::test]
async fn malformed_sigv4_date_is_rejected_not_panicking() {
    let server = TestServer::start().await;

    // A SigV4 header whose x-amz-date is shorter than 8 chars previously panicked the
    // handler via `&date[..8]`. It must now be a clean 403, and the server must survive.
    let response = server
        .client
        .get(server.object_url("any/object.txt"))
        .header(
            "Authorization",
            "AWS4-HMAC-SHA256 Credential=test_key/20240101/us-east-1/s3/aws4_request, \
             SignedHeaders=host, Signature=deadbeef",
        )
        .header("x-amz-date", "abc")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Server still serving afterwards (would fail if the task had panicked the runtime).
    let ok = server
        .client
        .head(format!("{}/{}/", server.base_url, server.bucket))
async fn list_buckets_nests_bucket_elements() {
    let server = TestServer::start().await;
    let response = server
        .client
        .get(format!("{}/", server.base_url))
        .header("Authorization", &server.auth_header)
async fn bucket_routes_work_without_trailing_slash() {
    let server = TestServer::start().await;

    // ListObjects on the bare bucket path (the aws-cli/boto3 default) must not 404.
    let list = server
        .client
        .get(format!("{}/{}", server.base_url, server.bucket))
        .header("Authorization", &server.auth_header)
        .query(&[("list-type", "2")])
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);

    // HeadBucket on the bare bucket path.
    let head = server
        .client
        .head(format!("{}/{}", server.base_url, server.bucket))
        .header("Authorization", &server.auth_header)
        .send()
        .await
        .unwrap();
    assert_eq!(head.status(), StatusCode::OK);

    // A missing bucket still returns 404.
    let missing = server
        .client
        .head(format!("{}/no-such-bucket", server.base_url))
        .header("Authorization", &server.auth_header)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
async fn presigned_url_is_rejected_after_it_expires() {
    let server = TestServer::start().await;
    let source_file = server._source_dir.path().join("exp.txt");
    fs::write(&source_file, b"expiring").await.unwrap();
    server.write("exp/object.txt", &source_file).await;

    // Presign a GET with a 1-second lifetime.
    let presign = server
        .client
        .post(format!("{}/_admin/presign", server.base_url))
        .header("Authorization", &server.auth_header)
        .query(&[
            ("bucket", server.bucket.as_str()),
            ("key", "exp/object.txt"),
            ("method", "GET"),
            ("expires", "1"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(presign.status(), StatusCode::OK);
    let url = presign.text().await.unwrap();

    // Once the lifetime lapses, the (still perfectly-signed) URL must be rejected.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let expired = server.client.get(&url).send().await.unwrap();
    assert_eq!(expired.status(), StatusCode::FORBIDDEN);
async fn put_object_returns_matching_etag_and_leaves_no_temp_files() {
    let server = TestServer::start().await;
    let source_file = server._source_dir.path().join("atomic.txt");
    fs::write(&source_file, b"atomic upload").await.unwrap();

    let response = server
        .client
        .put(server.object_url("atomic/object.txt"))
        .header("Authorization", &server.auth_header)
        .body(fs::read(&source_file).await.unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let xml = response.text().await.unwrap();

    // S3 SDKs parse ListBuckets as <Buckets><Bucket><Name>…</Name></Bucket></Buckets>;
    // without the <Bucket> wrapper they see zero buckets.
    assert!(xml.contains("<Buckets><Bucket>"), "missing <Bucket> wrapper: {xml}");
    assert!(xml.contains("<Name>test-bucket</Name>"), "missing bucket name: {xml}");
    assert!(xml.contains("</Bucket></Buckets>"), "missing closing wrappers: {xml}");

    // PutObject must return an ETag, and it must match a subsequent HEAD.
    let put_etag = response
        .headers()
        .get("etag")
        .expect("PutObject response is missing the ETag header")
        .to_str()
        .unwrap()
        .trim_matches('"')
        .to_string();
    assert_eq!(put_etag, server.head_etag("atomic/object.txt").await);

    // The atomic temp-file + rename must not leave any temporary files behind.
    let mut names = Vec::new();
    let mut dir = fs::read_dir(server._storage_dir.path().join("atomic"))
        .await
        .unwrap();
    while let Some(entry) = dir.next_entry().await.unwrap() {
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    assert_eq!(names, vec!["object.txt".to_string()]);
}

#[tokio::test]
async fn put_object_overwrite_fully_replaces_content() {
    let server = TestServer::start().await;
    let big = server._source_dir.path().join("big.txt");
    let small = server._source_dir.path().join("small.txt");
    fs::write(&big, vec![b'A'; 4096]).await.unwrap();
    fs::write(&small, b"tiny").await.unwrap();

    server.write("ov/object.txt", &big).await;
    assert_eq!(server.read("ov/object.txt").await.len(), 4096);

    server.write("ov/object.txt", &small).await;
    assert_eq!(server.read("ov/object.txt").await, b"tiny");
async fn put_object_acl_does_not_truncate_the_object() {
    let server = TestServer::start().await;
    let source_file = server._source_dir.path().join("acl.txt");
    fs::write(&source_file, b"acl must not clobber this").await.unwrap();
    server.write("acl/object.txt", &source_file).await;

    // A PutObjectAcl request (PUT key?acl) must be a no-op on the object body, not
    // overwrite it with the ACL payload.
    let response = server
        .client
        .put(format!("{}?acl", server.object_url("acl/object.txt")))
        .header("Authorization", &server.auth_header)
        .body("<AccessControlPolicy><Owner></Owner></AccessControlPolicy>")
async fn copy_object_onto_itself_is_rejected_and_preserves_content() {
    let server = TestServer::start().await;
    let source_file = server._source_dir.path().join("self.txt");
    fs::write(&source_file, b"do not lose me").await.unwrap();
    server.write("self/object.txt", &source_file).await;

    // Copying an object onto itself must be rejected with 400, not silently truncate
    // the object to 0 bytes (which is what a bare fs::copy would do).
    let response = server
        .client
        .put(server.object_url("self/object.txt"))
        .header("Authorization", &server.auth_header)
        .header(
            "x-amz-copy-source",
            format!("/{}/{}", server.bucket, "self/object.txt"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // The object must still hold its original bytes.
    assert_eq!(server.read("self/object.txt").await, b"do not lose me");
}

#[tokio::test]
async fn copy_object_result_etag_matches_destination_head() {
    let server = TestServer::start().await;
    let source_file = server._source_dir.path().join("etag.txt");
    fs::write(&source_file, b"etag consistency").await.unwrap();
    server.write("etag/source.txt", &source_file).await;

    let response = server
        .client
        .put(server.object_url("etag/dest.txt"))
        .header("Authorization", &server.auth_header)
        .header(
            "x-amz-copy-source",
            format!("/{}/{}", server.bucket, "etag/source.txt"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The object body is unchanged.
    assert_eq!(server.read("acl/object.txt").await, b"acl must not clobber this");
    let body = response.text().await.unwrap();
    let start = body.find("<ETag>").unwrap() + "<ETag>".len();
    let end = body[start..].find("</ETag>").unwrap() + start;
    let result_etag = body[start..end].trim_matches('"').to_string();

    // The ETag returned by CopyObject must match a subsequent HEAD of the new object.
    assert_eq!(result_etag, server.head_etag("etag/dest.txt").await);
}

#[tokio::test]
async fn bare_access_key_is_rejected_but_valid_basic_auth_passes() {
    let server = TestServer::start().await;
    let url = server.object_url("secured/object.txt");

    // A bare access key (no secret) must NOT authenticate. The access key is public
    // (it appears in every SigV4 Credential), so accepting it alone is a full bypass.
    let bare_key = server
        .client
        .get(&url)
        .header("Authorization", "test_key")
        .send()
        .await
        .unwrap();
    assert_eq!(bare_key.status(), StatusCode::FORBIDDEN);

    // The correct access key with a wrong secret must also be rejected.
    let wrong_secret = server
        .client
        .get(&url)
        .basic_auth("test_key", Some("not_the_secret"))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_secret.status(), StatusCode::FORBIDDEN);

    // No credentials at all must be rejected.
    let anonymous = server.client.get(&url).send().await.unwrap();
    assert_eq!(anonymous.status(), StatusCode::FORBIDDEN);

    // Valid Basic auth (access_key:secret_key) still works: 404 (not 403) proves it
    // passed the auth layer and reached the handler for a missing object.
    let valid = server
        .client
        .get(&url)
        .basic_auth("test_key", Some("test_secret"))
        .send()
        .await
        .unwrap();
    assert_eq!(valid.status(), StatusCode::NOT_FOUND);
}

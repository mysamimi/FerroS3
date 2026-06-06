#![allow(dead_code)]

use axum::{
    response::{Html, IntoResponse},
    Json,
};
use utoipa::{
    openapi::security::{Http, HttpAuthScheme, SecurityScheme},
    Modify, OpenApi,
};

use crate::error::S3Error;
use crate::handlers::admin::PresignParams;
use crate::handlers::bucket::{Bucket, ListAllMyBucketsResult, Owner};
use crate::handlers::list::{CommonPrefix, ListBucketResult, ListObjectsParams, ObjectContent};

#[derive(OpenApi)]
#[openapi(
    modifiers(&SecurityAddon),
    security(("basicAuth" = [])),
    paths(
        list_buckets_docs,
        head_bucket_docs,
        list_objects_docs,
        get_object_docs,
        head_object_docs,
        put_object_docs,
        delete_object_docs,
        generate_presigned_url_docs
    ),
    components(schemas(
        ListAllMyBucketsResult,
        Owner,
        Bucket,
        ListBucketResult,
        ObjectContent,
        CommonPrefix,
        S3Error
    )),
    tags(
        (name = "Buckets", description = "Bucket operations"),
        (name = "Objects", description = "Object operations"),
        (name = "Admin", description = "Administrative helpers")
    )
)]
pub struct ApiDoc;

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "basicAuth",
            SecurityScheme::Http(Http::new(HttpAuthScheme::Basic)),
        );
    }
}

pub async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

pub async fn swagger_ui_html() -> impl IntoResponse {
    Html(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>FerroS3 API Docs</title>
    <link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css" />
    <style>
      body { margin: 0; background: #fafafa; }
      #swagger-ui { max-width: 1440px; margin: 0 auto; }
    </style>
  </head>
  <body>
    <div id="swagger-ui"></div>
    <script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
    <script>
      window.onload = () => {
        window.ui = SwaggerUIBundle({
          url: '/openapi.json',
          dom_id: '#swagger-ui',
          deepLinking: true,
          presets: [SwaggerUIBundle.presets.apis],
          layout: 'BaseLayout'
        });
      };
    </script>
  </body>
</html>"#,
    )
}

#[utoipa::path(
    get,
    path = "/",
    tag = "Buckets",
    responses(
        (status = 200, description = "List configured buckets", body = ListAllMyBucketsResult, content_type = "application/xml"),
        (status = 403, description = "Access denied", body = S3Error, content_type = "application/xml")
    )
)]
async fn list_buckets_docs() {}

#[utoipa::path(
    head,
    path = "/{bucket}",
    tag = "Buckets",
    params(
        ("bucket" = String, Path, description = "Bucket name defined in config.yaml")
    ),
    responses(
        (status = 200, description = "Bucket exists"),
        (status = 404, description = "Bucket does not exist", body = S3Error, content_type = "application/xml"),
        (status = 403, description = "Access denied", body = S3Error, content_type = "application/xml")
    )
)]
async fn head_bucket_docs() {}

#[utoipa::path(
    get,
    path = "/{bucket}/",
    tag = "Objects",
    params(
        ("bucket" = String, Path, description = "Bucket name defined in config.yaml"),
        ListObjectsParams
    ),
    responses(
        (status = 200, description = "Bucket listing", body = ListBucketResult, content_type = "application/xml"),
        (status = 404, description = "Bucket does not exist", body = S3Error, content_type = "application/xml"),
        (status = 403, description = "Access denied", body = S3Error, content_type = "application/xml")
    )
)]
async fn list_objects_docs() {}

#[utoipa::path(
    get,
    path = "/{bucket}/{key}",
    tag = "Objects",
    params(
        ("bucket" = String, Path, description = "Bucket name defined in config.yaml"),
        ("key" = String, Path, allow_reserved, description = "Object key, URL-encode reserved characters when needed")
    ),
    responses(
        (status = 200, description = "Object body", body = Vec<u8>, content_type = "application/octet-stream"),
        (status = 206, description = "Partial object body", body = Vec<u8>, content_type = "application/octet-stream"),
        (status = 400, description = "Invalid key encoding"),
        (status = 404, description = "Object not found", body = S3Error, content_type = "application/xml"),
        (status = 403, description = "Access denied", body = S3Error, content_type = "application/xml")
    )
)]
async fn get_object_docs() {}

#[utoipa::path(
    head,
    path = "/{bucket}/{key}",
    tag = "Objects",
    params(
        ("bucket" = String, Path, description = "Bucket name defined in config.yaml"),
        ("key" = String, Path, allow_reserved, description = "Object key, URL-encode reserved characters when needed")
    ),
    responses(
        (status = 200, description = "Object metadata"),
        (status = 400, description = "Invalid key encoding"),
        (status = 404, description = "Object not found", body = S3Error, content_type = "application/xml"),
        (status = 403, description = "Access denied", body = S3Error, content_type = "application/xml")
    )
)]
async fn head_object_docs() {}

#[utoipa::path(
    put,
    path = "/{bucket}/{key}",
    tag = "Objects",
    params(
        ("bucket" = String, Path, description = "Bucket name defined in config.yaml"),
        ("key" = String, Path, allow_reserved, description = "Object key, URL-encode reserved characters when needed")
    ),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses(
        (status = 200, description = "Object stored successfully"),
        (status = 400, description = "Invalid key encoding"),
        (status = 404, description = "Bucket does not exist", body = S3Error, content_type = "application/xml"),
        (status = 403, description = "Access denied", body = S3Error, content_type = "application/xml"),
        (status = 500, description = "Internal error", body = S3Error, content_type = "application/xml")
    )
)]
async fn put_object_docs() {}

#[utoipa::path(
    delete,
    path = "/{bucket}/{key}",
    tag = "Objects",
    params(
        ("bucket" = String, Path, description = "Bucket name defined in config.yaml"),
        ("key" = String, Path, allow_reserved, description = "Object key, URL-encode reserved characters when needed")
    ),
    responses(
        (status = 204, description = "Object deleted or already missing"),
        (status = 400, description = "Invalid key encoding"),
        (status = 404, description = "Bucket does not exist", body = S3Error, content_type = "application/xml"),
        (status = 403, description = "Access denied", body = S3Error, content_type = "application/xml")
    )
)]
async fn delete_object_docs() {}

#[utoipa::path(
    post,
    path = "/_admin/presign",
    tag = "Admin",
    params(PresignParams),
    responses(
        (status = 200, description = "Presigned URL", body = String, content_type = "text/plain"),
        (status = 500, description = "Auth is not configured", body = String, content_type = "text/plain")
    )
)]
async fn generate_presigned_url_docs() {}

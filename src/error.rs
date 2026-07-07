use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use quick_xml::se::to_string;
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename = "Error")]
pub struct S3Error {
    #[serde(rename = "Code")]
    pub code: String,
    #[serde(rename = "Message")]
    pub message: String,
    #[serde(rename = "Resource", skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(rename = "RequestId")]
    pub request_id: String,
}

pub enum S3ErrorType {
    NoSuchKey,
    NoSuchBucket,
    AccessDenied,
    InvalidRequest,
    InternalError,
    // Add more as needed
}

/// Generate a random request ID without using the `uuid` crate.
/// This reads 16 bytes from /dev/urandom (always available on FreeBSD and Linux)
/// and formats them as a UUID v4-style string. This avoids the getrandom(2)
/// syscall which is only available on FreeBSD 12+ and Linux 3.17+.
fn new_request_id() -> String {
    use std::io::Read;
    let mut buf = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    // Format as UUID v4: xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx
    buf[6] = (buf[6] & 0x0f) | 0x40; // version 4
    buf[8] = (buf[8] & 0x3f) | 0x80; // variant bits
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        buf[0], buf[1], buf[2], buf[3],
        buf[4], buf[5],
        buf[6], buf[7],
        buf[8], buf[9],
        buf[10], buf[11], buf[12], buf[13], buf[14], buf[15]
    )
}

impl S3ErrorType {
    pub fn to_response(&self, resource: Option<String>) -> Response {
        let (status, code, message) = match self {
            S3ErrorType::NoSuchKey => (
                StatusCode::NOT_FOUND,
                "NoSuchKey",
                "The specified key does not exist.",
            ),
            S3ErrorType::NoSuchBucket => (
                StatusCode::NOT_FOUND,
                "NoSuchBucket",
                "The specified bucket does not exist.",
            ),
            S3ErrorType::AccessDenied => (
                StatusCode::FORBIDDEN,
                "AccessDenied",
                "Access Denied.",
            ),
            S3ErrorType::InvalidRequest => (
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "This copy request is illegal because it is trying to copy an object to itself without changing the object's metadata, storage class, website redirect location or encryption attributes.",
            ),
            S3ErrorType::InternalError => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalError",
                "An internal error occurred.",
            ),
        };

        let err = S3Error {
            code: code.to_string(),
            message: message.to_string(),
            resource,
            request_id: new_request_id(),
        };

        let xml = to_string(&err).unwrap_or_default();
        (status, [("Content-Type", "application/xml")], xml).into_response()
    }
}

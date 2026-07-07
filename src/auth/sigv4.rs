use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

type HmacSha256 = Hmac<Sha256>;

pub struct SigV4Params<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a BTreeMap<String, String>,
    pub headers: &'a BTreeMap<String, String>,
    pub payload_hash: &'a str,
    pub _access_key: &'a str,
    pub secret_key: &'a str,
    pub region: &'a str,
    pub service: &'a str,
    pub date: &'a str, // Full date: 20240516T123000Z
}

pub fn verify_signature(params: SigV4Params, signature: &str) -> bool {
    // `date` is attacker-controlled (x-amz-date). Reject anything that isn't at least a
    // full `YYYYMMDD` prefix before slicing `[..8]`, which would otherwise panic on a
    // short or multi-byte value.
    if params.date.len() < 8 || !params.date.is_char_boundary(8) {
        return false;
    }

    let canonical_request = create_canonical_request(&params);
    let string_to_sign = create_string_to_sign(&params, &canonical_request);
    let signing_key = create_signing_key(params.secret_key, &params.date[..8], params.region, params.service);

    let mut mac = HmacSha256::new_from_slice(&signing_key).expect("HMAC can take key of any size");
    mac.update(string_to_sign.as_bytes());
    let result = mac.finalize().into_bytes();
    let calculated_signature = hex::encode(result);

    constant_time_eq(calculated_signature.as_bytes(), signature.as_bytes())
}

/// Constant-time byte comparison, so verification time doesn't reveal how many leading
/// bytes of a guessed signature were correct (a timing side channel). Length is not
/// secret here — a hex SHA-256 signature is always 64 chars.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

fn create_canonical_request(params: &SigV4Params) -> String {
    let mut canonical = String::new();
    
    // 1. Method
    canonical.push_str(params.method);
    canonical.push('\n');
    
    // 2. Canonical URI
    canonical.push_str(params.path);
    canonical.push('\n');
    
    // 3. Canonical Query String
    let mut query_parts = Vec::new();
    for (k, v) in params.query {
        if k == "X-Amz-Signature" { continue; }
        query_parts.push(format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)));
    }
    canonical.push_str(&query_parts.join("&"));
    canonical.push('\n');
    
    // 4. Canonical Headers
    let mut signed_headers_list = Vec::new();
    for (k, v) in params.headers {
        let k_lower = k.to_lowercase();
        canonical.push_str(&k_lower);
        canonical.push(':');
        canonical.push_str(v.trim());
        canonical.push('\n');
        signed_headers_list.push(k_lower);
    }
    canonical.push('\n');
    
    // 5. Signed Headers List
    canonical.push_str(&signed_headers_list.join(";"));
    canonical.push('\n');
    
    // 6. Payload Hash
    canonical.push_str(params.payload_hash);
    
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    hex::encode(hasher.finalize())
}

fn create_string_to_sign(params: &SigV4Params, canonical_request_hash: &str) -> String {
    let mut s = String::new();
    s.push_str("AWS4-HMAC-SHA256\n");
    s.push_str(params.date);
    s.push('\n');
    
    // Credential Scope
    s.push_str(&params.date[..8]);
    s.push('/');
    s.push_str(params.region);
    s.push('/');
    s.push_str(params.service);
    s.push_str("/aws4_request\n");
    
    s.push_str(canonical_request_hash);
    s
}

fn create_signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_secret = format!("AWS4{}", secret);
    let k_date = hmac_sha256(k_secret.as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

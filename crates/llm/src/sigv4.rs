//! AWS Signature Version 4 for single requests with an in-memory body.

use hmac::{KeyInit, Mac};
use sha2::{Digest, Sha256};

#[derive(Clone, PartialEq, Eq)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for AwsCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsCredentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

pub struct SigningInput<'a> {
    pub method: &'a str,
    /// Already URI-encoded path, e.g. `/anthropic/v1/messages`.
    pub path: &'a str,
    /// Canonical (sorted, encoded) query string; empty when there is none.
    pub query: &'a str,
    /// Headers to sign, including `host` and `x-amz-date`.
    pub headers: &'a [(String, String)],
    pub payload_sha256: &'a str,
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = hmac::Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

pub(crate) fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date);
    let k_region = hmac(&k_date, region);
    let k_service = hmac(&k_region, service);
    hmac(&k_service, "aws4_request")
}

/// Returns the `Authorization` header value. `amz_date` is `YYYYMMDDTHHMMSSZ`
/// and must equal the signed `x-amz-date` header.
pub fn authorization(
    creds: &AwsCredentials,
    region: &str,
    service: &str,
    amz_date: &str,
    input: &SigningInput,
) -> String {
    let mut headers: Vec<(String, String)> = input
        .headers
        .iter()
        .map(|(k, v)| {
            let value = v.split_whitespace().collect::<Vec<_>>().join(" ");
            (k.to_ascii_lowercase(), value)
        })
        .collect();
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_request = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{}",
        input.method, input.path, input.query, input.payload_sha256
    );
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let key = signing_key(&creds.secret_access_key, date, region, service);
    let signature = hex(&hmac(&key, &string_to_sign));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

    #[test]
    fn signing_key_matches_aws_documented_example() {
        assert_eq!(
            hex(&signing_key(SECRET, "20120215", "us-east-1", "iam")),
            "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d"
        );
    }

    #[test]
    fn get_vanilla_matches_aws_test_suite() {
        let creds = AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: SECRET.into(),
            session_token: None,
        };
        let headers = [
            ("Host".to_string(), "example.amazonaws.com".to_string()),
            ("X-Amz-Date".to_string(), "20150830T123600Z".to_string()),
        ];
        let auth = authorization(
            &creds,
            "us-east-1",
            "service",
            "20150830T123600Z",
            &SigningInput {
                method: "GET",
                path: "/",
                query: "",
                headers: &headers,
                payload_sha256: &sha256_hex(b""),
            },
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn debug_hides_secrets() {
        let c = AwsCredentials {
            access_key_id: "AKIA".into(),
            secret_access_key: "very-secret".into(),
            session_token: Some("tok-secret".into()),
        };
        let s = format!("{c:?}");
        assert!(!s.contains("very-secret") && !s.contains("tok-secret"));
    }
}

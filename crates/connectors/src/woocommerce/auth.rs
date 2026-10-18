//! WooCommerce REST authentication. Over HTTPS it uses HTTP Basic with the
//! consumer key and secret; over plain HTTP WooCommerce only accepts one-legged
//! OAuth 1.0a, signed the way `WC_REST_Authentication` verifies it.

use base64::prelude::*;
use hmac::{KeyInit, Mac};
use sha2::Sha256;

use crate::transport::{HttpRequest, Method};

#[derive(Clone)]
pub struct WooAuth {
    pub consumer_key: String,
    pub consumer_secret: String,
}

impl std::fmt::Debug for WooAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WooAuth(<redacted>)")
    }
}

/// RFC 3986 percent-encoding, equivalent to PHP's `rawurlencode`.
pub(crate) fn rawurlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn method_name(m: Method) -> &'static str {
    match m {
        Method::Get => "GET",
        Method::Post => "POST",
        Method::Put => "PUT",
        Method::Patch => "PATCH",
        Method::Delete => "DELETE",
    }
}

/// The OAuth 1.0a base string as `WC_REST_Authentication` builds it: keys and
/// values are encoded, sorted, joined as `k=v`, and each pair encoded again,
/// so a space in a value ends up as `%2520`.
pub(crate) fn signature_base(
    method: Method,
    base_url: &str,
    params: &[(String, String)],
) -> String {
    let mut sorted: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (rawurlencode(k), rawurlencode(v)))
        .collect();
    sorted.sort();
    let joined = sorted
        .iter()
        .map(|(k, v)| rawurlencode(&format!("{k}={v}")))
        .collect::<Vec<_>>()
        .join("%26");
    format!(
        "{}&{}&{}",
        method_name(method),
        rawurlencode(base_url),
        joined
    )
}

pub(crate) fn sign(secret: &str, base: &str) -> String {
    let mut mac = hmac::Hmac::<Sha256>::new_from_slice(format!("{secret}&").as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(base.as_bytes());
    BASE64_STANDARD.encode(mac.finalize().into_bytes())
}

fn nonce() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("OS random source available");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl WooAuth {
    /// `url` must not contain a query string; `params` are the query parameters.
    pub fn apply(&self, method: Method, url: &str, params: Vec<(String, String)>) -> HttpRequest {
        if url.starts_with("https://") {
            let token =
                BASE64_STANDARD.encode(format!("{}:{}", self.consumer_key, self.consumer_secret));
            return HttpRequest::new(method, with_query(url, &params))
                .header("Authorization", format!("Basic {token}"));
        }
        let timestamp = chrono::Utc::now().timestamp().to_string();
        self.oauth_request(method, url, params, &timestamp, &nonce())
    }

    pub(crate) fn oauth_request(
        &self,
        method: Method,
        url: &str,
        mut params: Vec<(String, String)>,
        timestamp: &str,
        nonce: &str,
    ) -> HttpRequest {
        params.extend([
            ("oauth_consumer_key".into(), self.consumer_key.clone()),
            ("oauth_timestamp".into(), timestamp.into()),
            ("oauth_nonce".into(), nonce.into()),
            ("oauth_signature_method".into(), "HMAC-SHA256".into()),
        ]);
        let signature = sign(&self.consumer_secret, &signature_base(method, url, &params));
        params.push(("oauth_signature".into(), signature));
        HttpRequest::new(method, with_query(url, &params))
    }
}

fn with_query(url: &str, params: &[(String, String)]) -> String {
    if params.is_empty() {
        return url.to_string();
    }
    let query = params
        .iter()
        .map(|(k, v)| format!("{}={}", rawurlencode(k), rawurlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{url}?{query}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> WooAuth {
        WooAuth {
            consumer_key: "ck_test".into(),
            consumer_secret: "cs_test".into(),
        }
    }

    #[test]
    fn rawurlencode_matches_php() {
        assert_eq!(rawurlencode("a b+c/é~"), "a%20b%2Bc%2F%C3%A9~");
    }

    #[test]
    fn https_uses_basic_auth_and_no_oauth_params() {
        let req = auth().apply(
            Method::Get,
            "https://shop.test/wp-json/wc/v3/products",
            vec![("search".into(), "red shoe".into())],
        );
        assert_eq!(
            req.url,
            "https://shop.test/wp-json/wc/v3/products?search=red%20shoe"
        );
        assert_eq!(
            req.header_value("authorization"),
            Some(format!("Basic {}", BASE64_STANDARD.encode("ck_test:cs_test")).as_str())
        );
    }

    #[test]
    fn http_signs_with_woocommerce_base_string() {
        let req = auth().oauth_request(
            Method::Get,
            "http://shop.test/wp-json/wc/v3/products",
            vec![("per_page".into(), "5".into())],
            "1700000000",
            "abc",
        );
        assert!(req.header_value("authorization").is_none());
        let params = req.query_pairs();
        let sig = params
            .iter()
            .find(|(k, _)| k == "oauth_signature")
            .map(|(_, v)| v.clone())
            .unwrap();
        let base = "GET&http%3A%2F%2Fshop.test%2Fwp-json%2Fwc%2Fv3%2Fproducts&\
oauth_consumer_key%3Dck_test%26oauth_nonce%3Dabc%26oauth_signature_method%3DHMAC-SHA256\
%26oauth_timestamp%3D1700000000%26per_page%3D5";
        assert_eq!(
            signature_base(
                Method::Get,
                "http://shop.test/wp-json/wc/v3/products",
                &params
                    .iter()
                    .filter(|(k, _)| k != "oauth_signature")
                    .cloned()
                    .collect::<Vec<_>>()
            ),
            base
        );
        assert_eq!(sig, sign("cs_test", base));
    }

    #[test]
    fn values_with_spaces_are_double_encoded_in_the_base_string() {
        let base = signature_base(
            Method::Get,
            "http://shop.test/p",
            &[("search".into(), "blue hoodie".into())],
        );
        assert_eq!(
            base,
            "GET&http%3A%2F%2Fshop.test%2Fp&search%3Dblue%2520hoodie"
        );
    }

    #[test]
    fn debug_never_prints_secrets() {
        assert!(!format!("{:?}", auth()).contains("cs_test"));
    }
}

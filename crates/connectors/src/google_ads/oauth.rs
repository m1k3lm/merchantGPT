//! Google OAuth 2.0 for installed apps: the browser shows Google's consent
//! screen and redirects to a one-shot listener on 127.0.0.1, and the code is
//! exchanged with PKCE (RFC 7636). The refresh token is what gets stored.

use std::time::Duration;

use base64::prelude::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tools::CancellationToken;

use crate::transport::{HttpRequest, HttpTransport, Method};
use crate::ConnectorError;

pub const SCOPE: &str = "https://www.googleapis.com/auth/adwords";

#[derive(Clone, PartialEq, Eq)]
pub struct OAuthClient {
    pub client_id: String,
    pub client_secret: String,
}

impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClient")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub auth_url: String,
    pub token_url: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
        }
    }
}

fn random_urlsafe(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("OS random source available");
    BASE64_URL_SAFE_NO_PAD.encode(buf)
}

pub fn pkce_challenge(verifier: &str) -> String {
    BASE64_URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// application/x-www-form-urlencoded / RFC 3986 unreserved set.
pub(crate) fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn authorization_url(
    client: &OAuthClient,
    endpoints: &Endpoints,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> String {
    format!(
        "{}?{}",
        endpoints.auth_url,
        form(&[
            ("client_id", &client.client_id),
            ("redirect_uri", redirect_uri),
            ("response_type", "code"),
            ("scope", SCOPE),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            // offline + consent: Google only returns a refresh token this way.
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("state", state),
        ])
    )
}

#[derive(Clone, PartialEq, Eq)]
pub struct Tokens {
    pub access_token: String,
    pub expires_in: u64,
    pub refresh_token: Option<String>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("expires_in", &self.expires_in)
            .field("has_refresh_token", &self.refresh_token.is_some())
            .finish()
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn token_request(
    transport: &dyn HttpTransport,
    endpoints: &Endpoints,
    body: String,
) -> Result<Tokens, ConnectorError> {
    let req = HttpRequest::new(Method::Post, &endpoints.token_url)
        .header("Content-Type", "application/x-www-form-urlencoded");
    let req = HttpRequest {
        body: Some(body),
        ..req
    };
    let resp = transport.send(req).await?;
    let parsed: TokenResponse = serde_json::from_str(&resp.body)
        .map_err(|e| ConnectorError::InvalidResponse(format!("Google token response: {e}")))?;
    if let Some(error) = parsed.error {
        let detail = parsed.error_description.unwrap_or_default();
        return Err(ConnectorError::Auth(
            format!("Google refused the sign-in ({error}) {detail}")
                .trim()
                .to_string(),
        ));
    }
    Ok(Tokens {
        access_token: parsed
            .access_token
            .ok_or_else(|| ConnectorError::InvalidResponse("no access_token".into()))?,
        expires_in: parsed.expires_in.unwrap_or(3600),
        refresh_token: parsed.refresh_token,
    })
}

pub async fn refresh(
    transport: &dyn HttpTransport,
    endpoints: &Endpoints,
    client: &OAuthClient,
    refresh_token: &str,
) -> Result<Tokens, ConnectorError> {
    token_request(
        transport,
        endpoints,
        form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &client.client_id),
            ("client_secret", &client.client_secret),
        ]),
    )
    .await
}

/// Parses `GET /path?query HTTP/1.1` into the query parameters.
fn query_params(request: &str) -> Vec<(String, String)> {
    let target = request.split_whitespace().nth(1).unwrap_or("");
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

const DONE_PAGE: &str = "<html><body style=\"font-family:sans-serif\"><h2>merchantGPT is connected to Google</h2><p>You can close this tab and return to the app.</p></body></html>";
const FAILED_PAGE: &str = "<html><body style=\"font-family:sans-serif\"><h2>Google sign-in did not complete</h2><p>Return to merchantGPT and try again.</p></body></html>";

async fn respond(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Runs the whole sign-in: opens the consent page through `open_browser`,
/// waits for Google's redirect, and exchanges the code for tokens.
pub async fn authorize(
    transport: &dyn HttpTransport,
    endpoints: &Endpoints,
    client: &OAuthClient,
    open_browser: impl FnOnce(&str),
    cancel: CancellationToken,
    timeout: Duration,
) -> Result<Tokens, ConnectorError> {
    let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| {
        ConnectorError::Transport(format!("cannot listen for the Google redirect: {e}"))
    })?;
    let port = listener
        .local_addr()
        .map_err(|e| ConnectorError::Transport(e.to_string()))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}");
    let verifier = random_urlsafe(48);
    let state = random_urlsafe(24);
    open_browser(&authorization_url(
        client,
        endpoints,
        &redirect_uri,
        &state,
        &pkce_challenge(&verifier),
    ));

    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let code = loop {
        let accepted = tokio::select! {
            a = listener.accept() => a,
            _ = cancel.cancelled() => return Err(ConnectorError::Auth("Google sign-in was cancelled".into())),
            _ = &mut deadline => return Err(ConnectorError::Auth("Google sign-in timed out; try again".into())),
        };
        let Ok((mut stream, _)) = accepted else {
            continue;
        };
        let mut buf = vec![0u8; 8192];
        let n = match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            _ => continue,
        };
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        let params = query_params(&request);
        let get = |k: &str| params.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        // Browsers also ask for /favicon.ico and similar; ignore those.
        if get("code").is_none() && get("error").is_none() {
            respond(&mut stream, "404 Not Found", "").await;
            continue;
        }
        if get("state").as_deref() != Some(state.as_str()) {
            respond(&mut stream, "400 Bad Request", FAILED_PAGE).await;
            return Err(ConnectorError::Auth(
                "the Google redirect did not match this sign-in; try again".into(),
            ));
        }
        if let Some(error) = get("error") {
            respond(&mut stream, "200 OK", FAILED_PAGE).await;
            return Err(ConnectorError::Auth(format!(
                "Google sign-in was not completed ({error})"
            )));
        }
        respond(&mut stream, "200 OK", DONE_PAGE).await;
        break get("code").unwrap_or_default();
    };

    let tokens = token_request(
        transport,
        endpoints,
        form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("client_id", &client.client_id),
            ("client_secret", &client.client_secret),
            ("code_verifier", &verifier),
        ]),
    )
    .await?;
    if tokens.refresh_token.is_none() {
        return Err(ConnectorError::Auth(
            "Google did not return a refresh token; remove merchantGPT's access at myaccount.google.com/permissions and sign in again".into(),
        ));
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{FakeTransport, HttpResponse};
    use std::sync::Arc;

    fn client() -> OAuthClient {
        OAuthClient {
            client_id: "cid.apps.googleusercontent.com".into(),
            client_secret: "csecret".into(),
        }
    }

    fn endpoints() -> Endpoints {
        Endpoints::default()
    }

    #[test]
    fn pkce_matches_rfc7636_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn authorization_url_requests_offline_ads_access_with_pkce() {
        let url = authorization_url(&client(), &endpoints(), "http://127.0.0.1:5555", "st", "ch");
        let parsed = reqwest::Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q["scope"], SCOPE);
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:5555");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["access_type"], "offline");
        assert_eq!(q["prompt"], "consent");
        assert_eq!(q["state"], "st");
        assert!(
            !url.contains("csecret"),
            "the secret never goes to the browser"
        );
    }

    #[test]
    fn redirect_query_is_decoded() {
        let p = query_params("GET /?code=4%2F0Ab_c&state=x+y HTTP/1.1\r\nHost: x\r\n");
        assert_eq!(
            p,
            vec![
                ("code".into(), "4/0Ab_c".into()),
                ("state".into(), "x y".into())
            ]
        );
    }

    /// Plays the browser: follows the consent URL straight to the redirect.
    fn browser(reply: &'static str) -> impl FnOnce(&str) {
        move |url: &str| {
            let parsed = reqwest::Url::parse(url).unwrap();
            let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
            let target = match reply {
                "wrong-state" => format!("{}/?code=abc&state=nope", q["redirect_uri"]),
                "denied" => format!(
                    "{}/?error=access_denied&state={}",
                    q["redirect_uri"], q["state"]
                ),
                _ => format!("{}/?code=4%2Fabc&state={}", q["redirect_uri"], q["state"]),
            };
            let favicon = format!("{}/favicon.ico", q["redirect_uri"]);
            tokio::spawn(async move {
                let c = reqwest::Client::new();
                let _ = c.get(favicon).send().await;
                let _ = c.get(target).send().await;
            });
        }
    }

    #[tokio::test]
    async fn sign_in_exchanges_the_code_with_the_pkce_verifier() {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Post,
            "/token",
            HttpResponse::new(
                200,
                r#"{"access_token":"ya29.a","expires_in":3599,"refresh_token":"1//refresh","scope":"https://www.googleapis.com/auth/adwords","token_type":"Bearer"}"#,
            ),
        );
        let tokens = authorize(
            t.as_ref(),
            &endpoints(),
            &client(),
            browser("ok"),
            CancellationToken::new(),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(tokens.refresh_token.as_deref(), Some("1//refresh"));
        let body = t.requests()[0].body.clone().unwrap();
        let form: std::collections::HashMap<String, String> = body
            .split('&')
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap();
                (decode(k), decode(v))
            })
            .collect();
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(form["code"], "4/abc");
        assert_eq!(form["client_secret"], "csecret");
        assert!(form["redirect_uri"].starts_with("http://127.0.0.1:"));
        assert!(form["code_verifier"].len() >= 43, "RFC 7636 minimum length");
    }

    #[tokio::test]
    async fn denied_consent_and_forged_redirects_fail_cleanly() {
        let t = Arc::new(FakeTransport::new());
        for (reply, expected) in [
            ("denied", "access_denied"),
            ("wrong-state", "did not match"),
        ] {
            let err = authorize(
                t.as_ref(),
                &endpoints(),
                &client(),
                browser(reply),
                CancellationToken::new(),
                Duration::from_secs(10),
            )
            .await
            .unwrap_err();
            assert!(err.to_string().contains(expected), "{err}");
        }
        assert!(
            t.requests().is_empty(),
            "no token exchange without a valid code"
        );
    }

    #[tokio::test]
    async fn cancelling_stops_waiting() {
        let t = FakeTransport::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = authorize(
            &t,
            &endpoints(),
            &client(),
            |_| {},
            cancel,
            Duration::from_secs(10),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("cancelled"));
    }

    #[tokio::test]
    async fn token_errors_are_reported() {
        let t = FakeTransport::new();
        t.on(
            Method::Post,
            "/token",
            HttpResponse::new(400, r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#),
        );
        let err = refresh(&t, &endpoints(), &client(), "1//old")
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ConnectorError::Auth(
                "Google refused the sign-in (invalid_grant) Token has been expired or revoked."
                    .into()
            )
        );
    }
}

//! HTTP behind a trait so connectors are testable without network access.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;

use crate::ConnectorError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    /// Safe to resend after a transient failure without double-applying.
    fn is_idempotent(self) -> bool {
        !matches!(self, Method::Post | Method::Patch)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

impl HttpRequest {
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn json(mut self, body: &serde_json::Value) -> Self {
        self.body = Some(body.to_string());
        self.header("Content-Type", "application/json")
    }

    pub fn header_value(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }

    pub fn path(&self) -> String {
        reqwest::Url::parse(&self.url)
            .map(|u| u.path().to_string())
            .unwrap_or_default()
    }

    pub fn query_pairs(&self) -> Vec<(String, String)> {
        reqwest::Url::parse(&self.url)
            .map(|u| u.query_pairs().into_owned().collect())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

pub const USER_AGENT: &str = concat!("merchantGPT/", env!("CARGO_PKG_VERSION"));

/// The error and every underlying cause ("connection refused", "certificate
/// not trusted", "operation timed out"...). `reqwest` only prints the top
/// level, which is just "error sending request". The URL is dropped because
/// it can carry credentials in the query string.
pub fn describe(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        let text = s.to_string();
        if !msg.contains(&text) {
            msg.push_str(": ");
            msg.push_str(&text);
        }
        source = s.source();
    }
    msg
}

#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// Returns any HTTP status as a response; only network-level failures are errors.
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, ConnectorError>;
}

pub struct ReqwestTransport {
    client: reqwest::Client,
    max_retries: u32,
    base_backoff: Duration,
}

impl ReqwestTransport {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            max_retries: 3,
            base_backoff: Duration::from_millis(500),
        }
    }

    pub fn with_backoff(mut self, base: Duration, max_retries: u32) -> Self {
        self.base_backoff = base;
        self.max_retries = max_retries;
        self
    }

    async fn send_once(&self, req: &HttpRequest) -> Result<HttpResponse, reqwest::Error> {
        let method = match req.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Put => reqwest::Method::PUT,
            Method::Patch => reqwest::Method::PATCH,
            Method::Delete => reqwest::Method::DELETE,
        };
        let mut builder = self
            .client
            .request(method, &req.url)
            // Magento order pages can take tens of seconds on large stores.
            .timeout(Duration::from_secs(60));
        for (k, v) in &req.headers {
            builder = builder.header(k, v);
        }
        if let Some(body) = &req.body {
            builder = builder.body(body.clone());
        }
        let resp = builder.send().await?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_string())))
            .collect();
        let body = resp.text().await?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }

    fn backoff(&self, attempt: u32, retry_after: Option<&str>) -> Duration {
        let hinted = retry_after
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        hinted
            .unwrap_or(self.base_backoff * 2u32.pow(attempt))
            .min(Duration::from_secs(10))
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new(
            reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .build()
                .expect("static client configuration"),
        )
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, ConnectorError> {
        let retryable = req.method.is_idempotent();
        let mut attempt = 0;
        loop {
            let result = self.send_once(&req).await;
            let transient = match &result {
                Ok(r) => matches!(r.status, 429 | 502 | 503 | 504),
                // `is_request` covers a pooled connection the server already
                // closed: nothing was processed, so a resend is safe here.
                Err(e) => e.is_connect() || e.is_timeout() || e.is_request(),
            };
            if !(retryable && transient && attempt < self.max_retries) {
                // Without_url keeps query-string credentials out of error text.
                return result.map_err(|e| ConnectorError::Transport(describe(e)));
            }
            let retry_after = result.as_ref().ok().and_then(|r| r.header("retry-after"));
            tokio::time::sleep(self.backoff(attempt, retry_after)).await;
            attempt += 1;
        }
    }
}

/// In-memory transport for tests: canned responses by method and URL path,
/// and a log of every request sent.
#[derive(Default)]
pub struct FakeTransport {
    routes: Mutex<Vec<(Method, String, VecDeque<HttpResponse>)>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl FakeTransport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a response for `method path`. The last queued response for a
    /// route is repeated once the queue drains.
    pub fn on(&self, method: Method, path: &str, response: HttpResponse) -> &Self {
        let mut routes = self.routes.lock().unwrap();
        match routes
            .iter_mut()
            .find(|(m, p, _)| *m == method && p == path)
        {
            Some((_, _, q)) => q.push_back(response),
            None => routes.push((method, path.to_string(), VecDeque::from([response]))),
        }
        self
    }

    pub fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl HttpTransport for FakeTransport {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, ConnectorError> {
        let path = req.path();
        self.requests.lock().unwrap().push(req.clone());
        let mut routes = self.routes.lock().unwrap();
        let Some((_, _, queue)) = routes
            .iter_mut()
            .find(|(m, p, _)| *m == req.method && *p == path)
        else {
            return Ok(HttpResponse::new(404, r#"{"message":"no fake route"}"#));
        };
        Ok(if queue.len() > 1 {
            queue.pop_front().unwrap()
        } else {
            queue.front().cloned().unwrap()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_transport_routes_by_method_and_path_and_logs_requests() {
        let t = FakeTransport::new();
        t.on(Method::Get, "/a", HttpResponse::new(200, "first")).on(
            Method::Get,
            "/a",
            HttpResponse::new(200, "second"),
        );
        let get = |u: &str| HttpRequest::new(Method::Get, u);
        assert_eq!(t.send(get("https://x/a?q=1")).await.unwrap().body, "first");
        assert_eq!(t.send(get("https://x/a")).await.unwrap().body, "second");
        assert_eq!(t.send(get("https://x/a")).await.unwrap().body, "second");
        assert_eq!(t.send(get("https://x/b")).await.unwrap().status, 404);
        assert_eq!(t.requests().len(), 4);
        assert_eq!(
            t.requests()[0].query_pairs(),
            vec![("q".into(), "1".into())]
        );
    }

    /// Accepts connections and drops them at once, like a server closing a
    /// pooled connection. Returns the address and the accept counter.
    fn closing_server() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let count = std::sync::Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                c.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        });
        (format!("http://{addr}/rest/V1/orders?token=secret"), count)
    }

    #[tokio::test]
    async fn failed_sends_explain_the_cause_without_the_url_and_only_reads_retry() {
        use std::sync::atomic::Ordering;
        let t = ReqwestTransport::default().with_backoff(Duration::from_millis(1), 2);

        let (url, count) = closing_server();
        let err = t
            .send(HttpRequest::new(Method::Get, &url))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("could not reach the shop: "), "{msg}");
        assert!(
            msg.len() > "could not reach the shop: error sending request".len(),
            "{msg}"
        );
        assert!(
            !msg.contains("secret") && !msg.contains("127.0.0.1"),
            "{msg}"
        );
        assert_eq!(count.load(Ordering::SeqCst), 3, "GET is retried twice");

        let (url, count) = closing_server();
        t.send(HttpRequest::new(Method::Post, &url))
            .await
            .unwrap_err();
        assert_eq!(count.load(Ordering::SeqCst), 1, "POST is never resent");

        let refused = t
            .send(HttpRequest::new(Method::Get, "http://127.0.0.1:1/x"))
            .await
            .unwrap_err()
            .to_string()
            .to_lowercase();
        assert!(refused.contains("refused"), "{refused}");
    }

    #[test]
    fn backoff_honours_retry_after_and_caps() {
        let t = ReqwestTransport::default();
        assert_eq!(t.backoff(0, None), Duration::from_millis(500));
        assert_eq!(t.backoff(2, None), Duration::from_secs(2));
        assert_eq!(t.backoff(0, Some("3")), Duration::from_secs(3));
        assert_eq!(t.backoff(0, Some("600")), Duration::from_secs(10));
    }
}

//! Claude in Amazon Bedrock: the Messages API at
//! `https://bedrock-mantle.{region}.api.aws/anthropic/v1/messages`, streamed
//! as SSE, so it reuses the Anthropic codec. Model ids carry an `anthropic.`
//! prefix. Auth is either a Bedrock API key (bearer token sent as `x-api-key`)
//! or AWS credentials signed with SigV4 for the `bedrock-mantle` service.
//! Server-side fallbacks are not available on Bedrock.

use async_trait::async_trait;

use crate::anthropic::{encode, Decoder, Options, API_VERSION};
use crate::http::send_with_retry;
use crate::sigv4::{authorization, sha256_hex, AwsCredentials, SigningInput};
use crate::sse::{decode_stream, sse_events};
use crate::{
    ChatRequest, EventStream, LlmError, LlmProvider, ProviderConfig, ProviderId,
    OPT_AWS_ACCESS_KEY_ID, OPT_AWS_SESSION_TOKEN, OPT_REGION,
};

const SERVICE: &str = "bedrock-mantle";

#[derive(Debug, Clone)]
enum Auth {
    ApiKey(String),
    SigV4(AwsCredentials),
}

pub struct BedrockProvider {
    cfg: ProviderConfig,
    region: String,
    auth: Auth,
    http: reqwest::Client,
}

impl BedrockProvider {
    pub fn new(cfg: ProviderConfig, http: reqwest::Client) -> Result<Self, LlmError> {
        let region = cfg
            .option(OPT_REGION)
            .ok_or_else(|| LlmError::Config("enter the AWS region for Bedrock".into()))?
            .to_string();
        let auth = match cfg.option(OPT_AWS_ACCESS_KEY_ID) {
            Some(akid) => Auth::SigV4(AwsCredentials {
                access_key_id: akid.to_string(),
                secret_access_key: cfg.api_key.trim().to_string(),
                session_token: cfg.option(OPT_AWS_SESSION_TOKEN).map(str::to_string),
            }),
            None => Auth::ApiKey(cfg.api_key.trim().to_string()),
        };
        Ok(Self {
            cfg,
            region,
            auth,
            http,
        })
    }

    fn request(&self, url: &reqwest::Url, body: &[u8]) -> reqwest::RequestBuilder {
        let b = self
            .http
            .post(url.clone())
            .header("content-type", "application/json")
            .header("anthropic-version", API_VERSION)
            .body(body.to_vec());
        match &self.auth {
            Auth::ApiKey(key) => b.header("x-api-key", key),
            Auth::SigV4(creds) => {
                let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
                let payload_hash = sha256_hex(body);
                let host = match url.port() {
                    Some(p) => format!("{}:{p}", url.host_str().unwrap_or_default()),
                    None => url.host_str().unwrap_or_default().to_string(),
                };
                let mut signed = vec![
                    ("content-type".to_string(), "application/json".to_string()),
                    ("host".to_string(), host),
                    ("x-amz-content-sha256".to_string(), payload_hash.clone()),
                    ("x-amz-date".to_string(), amz_date.clone()),
                ];
                if let Some(t) = &creds.session_token {
                    signed.push(("x-amz-security-token".to_string(), t.clone()));
                }
                let auth = authorization(
                    creds,
                    &self.region,
                    SERVICE,
                    &amz_date,
                    &SigningInput {
                        method: "POST",
                        path: url.path(),
                        query: "",
                        headers: &signed,
                        payload_sha256: &payload_hash,
                    },
                );
                let mut b = b
                    .header("x-amz-date", amz_date)
                    .header("x-amz-content-sha256", payload_hash)
                    .header("authorization", auth);
                if let Some(t) = &creds.session_token {
                    b = b.header("x-amz-security-token", t);
                }
                b
            }
        }
    }
}

#[async_trait]
impl LlmProvider for BedrockProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Bedrock
    }

    async fn stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
        let url = reqwest::Url::parse(&format!("{}/v1/messages", self.cfg.base_url()))
            .map_err(|e| LlmError::Config(format!("invalid Bedrock URL: {e}")))?;
        let opts = Options {
            fallbacks: false,
            // Older Bedrock deployments reject the field.
            eager_input_streaming: false,
        };
        let body = serde_json::to_vec(&encode(&req, opts, ProviderId::Bedrock))
            .map_err(|e| LlmError::Decode(e.to_string()))?;
        // Re-signed on every retry, since the signature covers the timestamp.
        let resp = send_with_retry(|| self.request(&url, &body)).await?;
        let events = sse_events(Box::pin(resp.bytes_stream()));
        Ok(decode_stream(events, Decoder::default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn cfg(options: &[(&str, &str)]) -> ProviderConfig {
        ProviderConfig {
            provider: ProviderId::Bedrock,
            api_key: "secret".into(),
            base_url: None,
            model: "anthropic.claude-opus-5-5".into(),
            options: options
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn region_is_required_and_drives_the_endpoint() {
        assert!(matches!(
            BedrockProvider::new(cfg(&[]), reqwest::Client::new()),
            Err(LlmError::Config(_))
        ));
        let c = cfg(&[(OPT_REGION, "eu-west-1")]);
        assert_eq!(
            c.base_url(),
            "https://bedrock-mantle.eu-west-1.api.aws/anthropic"
        );
        assert!(c.is_official_endpoint());
    }

    #[test]
    fn access_key_id_selects_sigv4() {
        let p = BedrockProvider::new(
            cfg(&[(OPT_REGION, "eu-west-1"), (OPT_AWS_ACCESS_KEY_ID, "AKIA1")]),
            reqwest::Client::new(),
        )
        .unwrap();
        assert!(
            matches!(p.auth, Auth::SigV4(ref c) if c.secret_access_key == "secret" && c.session_token.is_none())
        );
        let p = BedrockProvider::new(cfg(&[(OPT_REGION, "eu-west-1")]), reqwest::Client::new())
            .unwrap();
        assert!(matches!(p.auth, Auth::ApiKey(ref k) if k == "secret"));
    }
}

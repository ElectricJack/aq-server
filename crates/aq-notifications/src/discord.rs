//! Fixed Discord v10 webhook adapter. Never log reqwest errors (the URL contains
//! a credential). No automatic retries or redirects; the durable owner decides.
use crate::{Error, FailureReason, Payload, Result, TransportOutcome, snowflake};
use reqwest::{Client, Url};
use serde::Deserialize;
use std::time::Duration;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThrottleScope {
    Destination,
    Provider,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Throttle {
    pub after_ms: u64,
    pub scope: ThrottleScope,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exchange {
    pub outcome: TransportOutcome,
    pub throttle: Option<Throttle>,
}
impl Exchange {
    fn unknown() -> Self {
        Self {
            outcome: TransportOutcome::Unknown,
            throttle: None,
        }
    }
}
/// Opaque protected endpoint. Deliberately no Serialize or Debug implementation.
pub struct Webhook {
    url: Url,
    id: String,
}
impl Webhook {
    pub fn parse(secret: &str) -> Result<Self> {
        Self::parse_inner(secret, false)
    }
    #[cfg(feature = "test-support")]
    pub fn loopback_fixture(secret: &str) -> Result<Self> {
        Self::parse_inner(secret, true)
    }
    fn parse_inner(secret: &str, fixture: bool) -> Result<Self> {
        if secret.len() > 4096 {
            return Err(Error::Invalid);
        }
        let mut url = Url::parse(secret).map_err(|_| Error::Invalid)?;
        if !allowed_endpoint(&url, fixture) {
            return Err(Error::Invalid);
        }
        // Discord hands out the unversioned `/api/webhooks/<id>/<token>` form;
        // pin it to the API version this transport speaks.
        if let Some(rest) = url.path().strip_prefix("/api/webhooks/") {
            let pinned = format!("/api/v10/webhooks/{rest}");
            url.set_path(&pinned);
        }
        let id = webhook_id(url.path()).ok_or(Error::Invalid)?.to_owned();
        url.set_query(Some("wait=true"));
        Ok(Self { url, id })
    }
}
fn allowed_endpoint(url: &Url, fixture: bool) -> bool {
    let discord =
        url.scheme() == "https" && url.host_str() == Some("discord.com") && url.port().is_none();
    let loopback = fixture
        && url.scheme() == "http"
        && url.host_str().is_some_and(|host| {
            host.parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        });
    (discord || loopback) && crate::bare_endpoint(url)
}
fn webhook_id(path: &str) -> Option<&str> {
    let path = path.strip_prefix("/api/v10/webhooks/")?;
    let (id, token) = path.split_once('/')?;
    let valid_token = (16..=256).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b));
    (snowflake(id) && valid_token).then_some(id)
}
pub struct Discord {
    client: Client,
}
impl Discord {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| Error::Invalid)?;
        Ok(Self { client })
    }
    pub async fn send(&self, endpoint: &Webhook, payload: &Payload) -> Result<Exchange> {
        let body = payload.wire()?;
        let response = self
            .client
            .post(endpoint.url.clone())
            .json(&body)
            .send()
            .await;
        let Ok(response) = response else {
            return Ok(Exchange::unknown());
        };
        let status = response.status().as_u16();
        let limits = RateHeaders::read(response.headers());
        let bytes = bounded_body(response).await;
        Ok(limits.exchange(status, bytes.as_deref(), &endpoint.id))
    }
}

async fn bounded_body(mut response: reqwest::Response) -> Option<Vec<u8>> {
    const MAX_BYTES: usize = 65536;
    if response
        .content_length()
        .is_some_and(|n| n > MAX_BYTES as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > MAX_BYTES - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    Some(bytes)
}

struct RateHeaders {
    global: bool,
    reset: Option<u64>,
    retry: Option<u64>,
}
impl RateHeaders {
    fn read(headers: &reqwest::header::HeaderMap) -> Self {
        let delay = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(seconds_ms)
        };
        let global = headers
            .get("x-ratelimit-global")
            .is_some_and(|v| v == "true")
            || headers
                .get("x-ratelimit-scope")
                .is_some_and(|v| v == "global");
        let exhausted = headers
            .get("x-ratelimit-remaining")
            .is_some_and(|v| v == "0");
        let reset = exhausted
            .then(|| delay("x-ratelimit-reset-after"))
            .flatten();
        Self {
            global,
            reset,
            retry: delay("retry-after"),
        }
    }
    fn exchange(&self, status: u16, bytes: Option<&[u8]>, webhook_id: &str) -> Exchange {
        let outcome = match (status, bytes) {
            (_, None) => TransportOutcome::Unknown,
            (200, Some(bytes)) => acceptance(bytes, webhook_id),
            (429, Some(bytes)) => {
                if let Some(exchange) = self.limited(bytes) {
                    return exchange;
                }
                TransportOutcome::Unknown
            }
            (400 | 413 | 422, _) => TransportOutcome::Permanent(FailureReason::InvalidPayload),
            (401 | 403 | 404 | 410, _) => {
                TransportOutcome::Permanent(FailureReason::DestinationUnavailable)
            }
            // A service failure may arrive after Discord accepted the send.
            _ => TransportOutcome::Unknown,
        };
        Exchange {
            outcome,
            throttle: self.reset.map(|ms| Throttle::new(ms, self.global)),
        }
    }
    fn limited(&self, bytes: &[u8]) -> Option<Exchange> {
        #[derive(Deserialize)]
        struct RateLimit {
            retry_after: f64,
            global: bool,
        }
        let parsed = serde_json::from_slice::<RateLimit>(bytes).ok();
        let retry_after_ms = parsed
            .as_ref()
            .and_then(|r| seconds_value_ms(r.retry_after))
            .into_iter()
            .chain(self.retry)
            .chain(self.reset)
            .max()?;
        let global = self.global || parsed.is_some_and(|r| r.global);
        Some(Exchange {
            outcome: TransportOutcome::RateLimited {
                retry_after_ms,
                global,
            },
            throttle: Some(Throttle::new(retry_after_ms, global)),
        })
    }
}
impl Throttle {
    fn new(after_ms: u64, global: bool) -> Self {
        let scope = if global {
            ThrottleScope::Provider
        } else {
            ThrottleScope::Destination
        };
        Self { after_ms, scope }
    }
}
fn acceptance(bytes: &[u8], webhook_id: &str) -> TransportOutcome {
    #[derive(Deserialize)]
    struct Receipt {
        id: String,
        webhook_id: String,
    }
    match serde_json::from_slice::<Receipt>(bytes) {
        Ok(receipt) if snowflake(&receipt.id) && receipt.webhook_id == webhook_id => {
            TransportOutcome::Accepted {
                message_id: receipt.id,
            }
        }
        _ => TransportOutcome::Unknown,
    }
}
fn seconds_ms(v: &str) -> Option<u64> {
    seconds_value_ms(v.parse().ok()?)
}
fn seconds_value_ms(v: f64) -> Option<u64> {
    // Values above our automatic horizon are retained as a stop condition, never
    // clamped downward into an earlier retry. Absolute overflow is refused.
    if !v.is_finite() || !(0.0..=31_536_000.0).contains(&v) {
        None
    } else {
        Some((v * 1000.0).ceil() as u64)
    }
}

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
        let production = url.scheme() == "https"
            && url.host_str() == Some("discord.com")
            && url.port().is_none();
        let local = fixture
            && url.scheme() == "http"
            && url
                .host_str()
                .is_some_and(|h| h.parse::<std::net::IpAddr>().is_ok_and(|a| a.is_loopback()));
        if !(production || local)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::Invalid);
        }
        // Discord hands out the unversioned `/api/webhooks/<id>/<token>` form;
        // pin it to the API version this transport speaks.
        if let Some(rest) = url.path().strip_prefix("/api/webhooks/") {
            let pinned = format!("/api/v10/webhooks/{rest}");
            url.set_path(&pinned);
        }
        let parts: Vec<_> = url.path().split('/').collect();
        if parts.len() != 6
            || parts[..4] != ["", "api", "v10", "webhooks"]
            || !snowflake(parts[4])
            || !(16..=256).contains(&parts[5].len())
            || !parts[5]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Err(Error::Invalid);
        }
        let id = parts[4].to_owned();
        url.set_query(Some("wait=true"));
        Ok(Self { url, id })
    }
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
        let Ok(mut response) = response else {
            return Ok(Exchange::unknown());
        };
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let delay_header = |name: &str| {
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
        let reset = if headers
            .get("x-ratelimit-remaining")
            .is_some_and(|v| v == "0")
        {
            delay_header("x-ratelimit-reset-after")
        } else {
            None
        };
        let throttle = reset.map(|after_ms| Throttle {
            after_ms,
            scope: if global {
                ThrottleScope::Provider
            } else {
                ThrottleScope::Destination
            },
        });
        if response.content_length().is_some_and(|n| n > 65536) {
            return Ok(Exchange {
                outcome: TransportOutcome::Unknown,
                throttle,
            });
        }
        let mut bytes = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) if bytes.len() + chunk.len() <= 65536 => {
                    bytes.extend_from_slice(&chunk)
                }
                Ok(None) => break,
                _ => {
                    return Ok(Exchange {
                        outcome: TransportOutcome::Unknown,
                        throttle,
                    });
                }
            }
        }
        let outcome = match status {
            200 => {
                #[derive(Deserialize)]
                struct Receipt {
                    id: String,
                    webhook_id: String,
                }
                match serde_json::from_slice::<Receipt>(&bytes) {
                    Ok(r) if snowflake(&r.id) && r.webhook_id == endpoint.id => {
                        TransportOutcome::Accepted { message_id: r.id }
                    }
                    _ => TransportOutcome::Unknown,
                }
            }
            429 => {
                #[derive(Deserialize)]
                struct RateLimit {
                    retry_after: f64,
                    global: bool,
                }
                let parsed = serde_json::from_slice::<RateLimit>(&bytes).ok();
                let delay = parsed
                    .as_ref()
                    .and_then(|r| seconds_value_ms(r.retry_after))
                    .into_iter()
                    .chain(delay_header("retry-after"))
                    .chain(reset)
                    .max();
                if let Some(retry_after_ms) = delay {
                    let global = global || parsed.is_some_and(|r| r.global);
                    return Ok(Exchange {
                        outcome: TransportOutcome::RateLimited {
                            retry_after_ms,
                            global,
                        },
                        throttle: Some(Throttle {
                            after_ms: retry_after_ms,
                            scope: if global {
                                ThrottleScope::Provider
                            } else {
                                ThrottleScope::Destination
                            },
                        }),
                    });
                } else {
                    TransportOutcome::Unknown
                }
            }
            400 | 413 | 422 => TransportOutcome::Permanent(FailureReason::InvalidPayload),
            401 | 403 | 404 | 410 => {
                TransportOutcome::Permanent(FailureReason::DestinationUnavailable)
            }
            // Gateway/service failures may arrive after Discord accepted a send.
            // A retryable HTTP class does not prove absence of an external effect.
            _ => TransportOutcome::Unknown,
        };
        Ok(Exchange { outcome, throttle })
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

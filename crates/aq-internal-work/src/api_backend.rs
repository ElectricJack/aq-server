//! The API way of running internal work (spec 0012 §6): one OpenAI
//! Responses-compatible request per candidate, with a key read from a file.
//!
//! The key is read inside [`ApiBackend::run`], held in a zeroizing buffer,
//! sent as a sensitive `Authorization` header and dropped once the response
//! head arrives. Failure details are fixed strings: they never contain the
//! key, a header or the response body, because they are stored in a row.
use crate::{Claimed, Error, Reason, Result, Usage, validate};
use reqwest::{StatusCode, Url, header::HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

/// Largest provider response body read.
const MAX_RESPONSE_BYTES: usize = 1 << 20;
/// Largest key file accepted.
const MAX_SECRET_BYTES: u64 = 4096;

/// The optional `[internal_work]` section of the server config.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InternalWorkConfig {
    /// Absent means no API endpoints.
    #[serde(default)]
    pub api: Vec<ApiEndpoint>,
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_poll_ms() -> u64 {
    200
}

fn default_enabled() -> bool {
    true
}

/// Where and how one provider is called.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiEndpoint {
    pub provider: String,
    pub endpoint: String,
    /// Absolute path of an owner-only file holding the key.
    pub secret_file: PathBuf,
}

pub struct ApiBackend {
    endpoints: BTreeMap<String, (Url, PathBuf)>,
    client: reqwest::Client,
}

impl std::fmt::Debug for ApiBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiBackend")
            .field("providers", &self.endpoints.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl ApiBackend {
    pub fn new(endpoints: Vec<ApiEndpoint>) -> Result<Self> {
        let mut by_provider = BTreeMap::new();
        for e in endpoints {
            let url = Url::parse(&e.endpoint)
                .map_err(|_| invalid(&format!("invalid api endpoint for {}", e.provider)))?;
            let loopback = url
                .host_str()
                .map(|h| h.trim_start_matches('[').trim_end_matches(']'))
                .and_then(|h| h.parse::<std::net::IpAddr>().ok())
                .is_some_and(|a| a.is_loopback());
            // The key is only ever sent over TLS, or to a loopback endpoint.
            if e.provider.is_empty()
                || !(url.scheme() == "https" || url.scheme() == "http" && loopback)
                || url.host().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || !e.secret_file.is_absolute()
            {
                return Err(invalid(&format!(
                    "api endpoint for {:?} is not allowed",
                    e.provider
                )));
            }
            if by_provider
                .insert(e.provider.clone(), (url, e.secret_file))
                .is_some()
            {
                return Err(invalid(&format!(
                    "duplicate api endpoint for {}",
                    e.provider
                )));
            }
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|_| invalid("api client unavailable"))?;
        Ok(Self {
            endpoints: by_provider,
            client,
        })
    }

    /// Sends the claimed task's prompt and returns the model's raw text and
    /// the usage the provider reported.
    pub async fn run(
        &self,
        claimed: &Claimed,
    ) -> std::result::Result<(String, Option<Usage>), (Reason, String)> {
        let provider = &claimed.candidate.provider;
        let Some((url, secret_file)) = self.endpoints.get(provider) else {
            return Err((
                Reason::ProviderUnavailable,
                format!("no api endpoint configured for {provider}"),
            ));
        };
        let remaining_ms = claimed.deadline_at_ms - now_ms();
        if remaining_ms <= 0 {
            return Err((
                Reason::DeadlineExceeded,
                "deadline passed before send".into(),
            ));
        }
        let body = json!({
            "model": claimed.candidate.model,
            "input": validate::prompt(&claimed.request),
            "store": false,
        });
        let key = read_secret(secret_file).map_err(|_| {
            (
                Reason::ProviderUnavailable,
                format!("api key file for {provider} is unusable"),
            )
        })?;
        let mut header =
            HeaderValue::from_str(&format!("Bearer {}", key.as_str())).map_err(|_| {
                (
                    Reason::ProviderUnavailable,
                    format!("api key file for {provider} is unusable"),
                )
            })?;
        header.set_sensitive(true);
        drop(key);
        let sent = self
            .client
            .post(url.clone())
            .header(reqwest::header::AUTHORIZATION, header)
            .timeout(Duration::from_millis(remaining_ms as u64))
            .json(&body)
            .send()
            .await;
        let mut response = sent.map_err(|_| unavailable("request to the provider failed"))?;
        let status = response.status();
        if !status.is_success() {
            let reason = if status == StatusCode::TOO_MANY_REQUESTS
                || status == StatusCode::REQUEST_TIMEOUT
                || status.is_server_error()
            {
                Reason::ProviderUnavailable
            } else {
                Reason::ProviderError
            };
            return Err((
                reason,
                format!("provider answered HTTP {}", status.as_u16()),
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| unavailable("reading the provider response failed"))?
        {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err((
                    Reason::ProviderError,
                    "provider response exceeds 1 MiB".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        parse(&bytes)
    }
}

fn parse(body: &[u8]) -> std::result::Result<(String, Option<Usage>), (Reason, String)> {
    let bad = |what: &str| (Reason::ProviderError, format!("provider response {what}"));
    let response: Value = serde_json::from_slice(body).map_err(|_| bad("is not JSON"))?;
    if response["status"]
        .as_str()
        .is_some_and(|s| s != "completed")
    {
        return Err(bad("is not completed"));
    }
    let text = response["output"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "message")
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
        .find(|c| c["type"] == "output_text")
        .and_then(|c| c["text"].as_str())
        .ok_or_else(|| bad("has no output text"))?;
    let usage = match (
        response["usage"]["input_tokens"].as_u64(),
        response["usage"]["output_tokens"].as_u64(),
    ) {
        (Some(input_tokens), Some(output_tokens)) => Some(Usage {
            input_tokens,
            output_tokens,
        }),
        _ => None,
    };
    Ok((text.to_owned(), usage))
}

fn unavailable(detail: &str) -> (Reason, String) {
    (Reason::ProviderUnavailable, detail.into())
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// A key only from an absolute, owner-only, single-link, non-symlink file.
fn read_secret(path: &Path) -> std::result::Result<Zeroizing<String>, ()> {
    if !path.is_absolute() {
        return Err(());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| ())?;
    let m = file.metadata().map_err(|_| ())?;
    if !m.is_file()
        || m.mode() & 0o077 != 0
        || m.uid() != nix::unistd::geteuid().as_raw()
        || m.nlink() != 1
        || m.len() == 0
        || m.len() > MAX_SECRET_BYTES
    {
        return Err(());
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_SECRET_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    let text = std::str::from_utf8(&bytes).map_err(|_| ())?.trim();
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(());
    }
    Ok(Zeroizing::new(text.to_owned()))
}

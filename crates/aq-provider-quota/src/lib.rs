//! Router-owned quota collection. Credentials and upstream errors never leave this module.
pub mod inventory;
mod snapshot;
use aq_dashboard_contract::{
    Completeness, Observation, ProviderScope, ProviderUsage, Quality, QuotaState, QuotaWindow,
    Timestamp,
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Mutex as AsyncMutex;
use zeroize::Zeroizing;

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Account {
    pub provider: String,
    pub account_reference: String,
    pub enabled: bool,
    /// Startup key-presence fact only; not authentication/model readiness.
    pub provisionable: bool,
    pub credential_profiles: u32,
}

const ENDPOINT: &str = "https://chatgpt.com/backend-api/wham/usage";
const CACHE_MS: u64 = 30_000;
const MAX_BYTES: usize = 65_536;

/// Explicit operator mapping: the symbolic inventory account is not assumed to
/// equal the ChatGPT account identifier in the OAuth file.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexSource {
    pub provider_id: String,
    pub account_ref: String,
    #[serde(default)]
    pub chatgpt_account_id: String,
    #[serde(default)]
    pub auth_file: PathBuf,
    #[serde(default)]
    pub snapshot_file: Option<PathBuf>,
    #[serde(default)]
    pub account_label: Option<String>,
}
impl std::fmt::Debug for CodexSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexSource")
            .field("provider_id", &self.provider_id)
            .field("account_ref", &self.account_ref)
            .field("auth_file", &"<protected>")
            .finish_non_exhaustive()
    }
}
impl CodexSource {
    pub fn validate(&self) -> bool {
        if self
            .account_label
            .as_ref()
            .is_some_and(|label| !valid_text(label, 128))
        {
            return false;
        }
        if let Some(path) = &self.snapshot_file {
            return matches!(self.provider_id.as_str(), "openai" | "anthropic")
                && valid_text(&self.account_ref, 512)
                && path.is_absolute();
        }
        self.provider_id == "openai"
            && valid_text(&self.account_ref, 512)
            && valid_text(&self.chatgpt_account_id, 256)
            && self.auth_file.is_absolute()
    }
}
fn valid_text(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control)
}
fn time(ms: u64) -> Option<Timestamp> {
    Some(Timestamp(
        DateTime::<Utc>::from_timestamp_millis(i64::try_from(ms).ok()?)?
            .to_rfc3339_opts(SecondsFormat::Millis, true),
    ))
}
fn observation(now: u64, known: bool) -> Option<Observation> {
    Some(Observation {
        observed_at: time(now)?,
        // An "unknown" answer is itself fresh for one envelope interval; equal
        // timestamps rendered "Stale, observed 0s ago" (QA SO-08).
        fresh_until: time(now.checked_add(if known { CACHE_MS } else { 5_000 })?)?,
        source: if known {
            "codex_chatgpt_usage"
        } else {
            "provider_quota_unknown"
        }
        .into(),
        quality: if known {
            Quality::Reported
        } else {
            Quality::Unknown
        },
        coverage: if known {
            Completeness::Complete
        } else {
            Completeness::Unknown
        },
    })
}
fn unknown(id: String, label: String, now: u64) -> Option<QuotaWindow> {
    Some(QuotaWindow {
        id,
        label,
        starts_at: None,
        resets_at: None,
        used_percent: None,
        used: None,
        limit: None,
        unit: "percent".into(),
        state: QuotaState::Unknown,
        observation: observation(now, false)?,
    })
}
fn window(id: String, label: String, value: &Value, now: u64) -> Option<QuotaWindow> {
    if value.is_null() {
        return unknown(id, label, now);
    }
    let o = value.as_object()?;
    let p = o.get("used_percent")?.as_f64()?;
    if !p.is_finite() || !(0.0..=100.0).contains(&p) {
        return None;
    }
    let reset = match o.get("reset_at") {
        None | Some(Value::Null) => None,
        Some(v) => Some(time(v.as_u64()?.checked_mul(1000)?)?),
    };
    Some(QuotaWindow {
        id,
        label,
        starts_at: None,
        resets_at: reset,
        used_percent: Some(p),
        used: None,
        limit: None,
        unit: "percent".into(),
        state: QuotaState::Known,
        observation: observation(now, true)?,
    })
}
fn parse(body: &[u8], expected_account: &str, now: u64) -> Option<Vec<QuotaWindow>> {
    let root: Value = serde_json::from_slice(body).ok()?;
    let o = root.as_object()?;
    if let Some(account) = o.get("account_id").filter(|v| !v.is_null())
        && account.as_str()? != expected_account
    {
        return None;
    }
    let mut windows = Vec::new();
    let main = o.get("rate_limit").unwrap_or(&Value::Null);
    if !main.is_null() && !main.is_object() {
        return None;
    }
    for (key, label) in [
        ("primary_window", "Primary"),
        ("secondary_window", "Secondary"),
    ] {
        windows.push(window(
            key.into(),
            label.into(),
            main.get(key).unwrap_or(&Value::Null),
            now,
        )?);
    }
    if let Some(extra) = o.get("additional_rate_limits").filter(|v| !v.is_null()) {
        let extra = extra.as_array()?;
        if extra.len() > 8 {
            return None;
        }
        let mut seen = BTreeSet::new();
        for entry in extra {
            let feature = entry.get("metered_feature")?.as_str()?;
            let name = entry.get("limit_name")?.as_str()?;
            if !valid_text(feature, 128) || !valid_text(name, 128) || !seen.insert((feature, name))
            {
                return None;
            }
            use sha2::Digest as _;
            let id = hex::encode(sha2::Sha256::digest(
                format!("{}:{}:{}", feature.len(), feature, name).as_bytes(),
            ));
            let rate = entry.get("rate_limit").unwrap_or(&Value::Null);
            if !rate.is_null() && !rate.is_object() {
                return None;
            }
            for (key, suffix) in [
                ("primary_window", "Primary"),
                ("secondary_window", "Secondary"),
            ] {
                windows.push(window(
                    format!("additional:{id}:{key}"),
                    format!("Additional quota {} — {suffix}", &id[..8]),
                    rate.get(key).unwrap_or(&Value::Null),
                    now,
                )?);
            }
        }
    }
    Some(windows)
}
fn auth(source: &CodexSource, now: u64) -> Option<Zeroizing<String>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(&source.auth_file)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != nix::unistd::geteuid().as_raw()
    {
        return None;
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return None;
    }
    let mut v: Value = serde_json::from_slice(&bytes).ok()?;
    let tokens = v.get_mut("tokens")?;
    if tokens.get("account_id")?.as_str()? != source.chatgpt_account_id {
        return None;
    }
    let Value::String(token) = tokens.get_mut("access_token")?.take() else {
        return None;
    };
    let token = Zeroizing::new(token);
    if !valid_text(&token, 16_384) || !token.is_ascii() {
        return None;
    }
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() == 3 {
        use base64::Engine as _;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[1])
            .ok()?;
        let claims: Value = serde_json::from_slice(&payload).ok()?;
        if claims
            .get("exp")
            .and_then(Value::as_u64)
            .is_none_or(|exp| exp <= now / 1000)
        {
            return None;
        }
    }
    Some(token)
}
#[derive(Clone)]
struct Cache {
    windows: Option<Vec<QuotaWindow>>,
    observed_at: u64,
    attempted_at: u64,
    failed: bool,
}
type Cell = Arc<AsyncMutex<Option<Cache>>>;
#[derive(Clone)]
pub struct Collector {
    client: Option<reqwest::Client>,
    endpoint: String,
    cells: Arc<Mutex<BTreeMap<(String, String), Cell>>>,
}
impl Collector {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .timeout(Duration::from_millis(900))
                .build()
                .ok(),
            endpoint: ENDPOINT.into(),
            cells: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
    async fn request(&self, source: CodexSource, now: u64) -> Option<Vec<QuotaWindow>> {
        if source.snapshot_file.is_some() {
            return tokio::task::spawn_blocking(move || snapshot::read(&source, now))
                .await
                .ok()?;
        }
        let file_source = source.clone();
        let token = tokio::time::timeout(
            Duration::from_millis(500),
            tokio::task::spawn_blocking(move || auth(&file_source, now)),
        )
        .await
        .ok()?
        .ok()??;
        let mut response = self
            .client
            .as_ref()?
            .get(&self.endpoint)
            .bearer_auth(token.as_str())
            .header("ChatGPT-Account-Id", &source.chatgpt_account_id)
            .header(reqwest::header::USER_AGENT, "codex-cli")
            .send()
            .await
            .ok()?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|n| n > MAX_BYTES as u64)
        {
            return None;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if bytes.len() + chunk.len() > MAX_BYTES {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        parse(&bytes, &source.chatgpt_account_id, now)
    }
    async fn account(
        &self,
        account: Account,
        source: Option<CodexSource>,
        now: u64,
    ) -> (ProviderUsage, bool) {
        let label = source
            .as_ref()
            .and_then(|s| s.account_label.clone())
            .unwrap_or_else(|| account.account_reference.clone());
        let mut partial = false;
        let cell = self.cells.lock().ok().and_then(|mut cells| {
            if cells.len() >= 32
                && !cells
                    .contains_key(&(account.provider.clone(), account.account_reference.clone()))
            {
                return None;
            }
            Some(
                cells
                    .entry((account.provider.clone(), account.account_reference.clone()))
                    .or_default()
                    .clone(),
            )
        });
        let mut value = None;
        if let (Some(cell), Some(source)) = (cell, source)
            && let Ok(mut cache) =
                tokio::time::timeout(Duration::from_millis(1450), cell.lock()).await
        {
            let cached = cache.as_ref().filter(|c| {
                now >= c.attempted_at
                    && now - c.attempted_at < if c.failed { 5_000 } else { CACHE_MS }
            });
            if let Some(c) = cached {
                value = c.windows.clone();
                partial |= c.failed;
            } else if let Some(windows) = self.request(source, now).await {
                *cache = Some(Cache {
                    windows: Some(windows.clone()),
                    observed_at: now,
                    attempted_at: now,
                    failed: false,
                });
                value = Some(windows);
            } else {
                let previous = cache
                    .as_ref()
                    .filter(|c| now >= c.observed_at)
                    .and_then(|c| c.windows.clone());
                let observed_at = cache.as_ref().map_or(now, |c| c.observed_at);
                *cache = Some(Cache {
                    windows: previous.clone(),
                    observed_at,
                    attempted_at: now,
                    failed: true,
                });
                value = previous;
                partial = true;
            }
        }
        let windows = value.unwrap_or_else(|| {
            partial = true;
            unknown("unknown".into(), "Unavailable".into(), now)
                .into_iter()
                .collect()
        });
        if windows.iter().any(|w| {
            !matches!(w.state, QuotaState::Known)
                || DateTime::parse_from_rfc3339(&w.observation.fresh_until.0)
                    .ok()
                    .is_none_or(|at| at.timestamp_millis() < now as i64)
        }) {
            partial = true;
        }
        let preferred_window_id = windows
            .iter()
            .find(|w| matches!(w.state, QuotaState::Known))
            .or_else(|| windows.first())
            .map(|w| w.id.clone());
        (
            ProviderUsage {
                provider_id: account.provider,
                account_label: label,
                account_ref: account.account_reference,
                scope: ProviderScope::ProviderAccount,
                windows,
                preferred_window_id,
                availability_ref: None,
            },
            partial,
        )
    }
    pub async fn collect(
        &self,
        mut accounts: Vec<Account>,
        sources: &[CodexSource],
        now: u64,
    ) -> (Vec<ProviderUsage>, bool) {
        for source in sources
            .iter()
            .filter(|s| s.snapshot_file.is_some() && s.validate())
            .take(32)
        {
            if !accounts.iter().any(|a| {
                a.provider == source.provider_id && a.account_reference == source.account_ref
            }) {
                accounts.push(Account {
                    provider: source.provider_id.clone(),
                    account_reference: source.account_ref.clone(),
                    enabled: true,
                    provisionable: false,
                    credential_profiles: 0,
                });
            }
        }
        let mut pending = tokio::task::JoinSet::new();
        for (index, account) in accounts.into_iter().enumerate() {
            let matches = sources
                .iter()
                .filter(|s| {
                    s.provider_id == account.provider
                        && s.account_ref == account.account_reference
                        && s.validate()
                })
                .collect::<Vec<_>>();
            let source = if let [source] = matches.as_slice() {
                Some((*source).clone())
            } else {
                None
            };
            let collector = self.clone();
            pending.spawn(async move { (index, collector.account(account, source, now).await) });
        }
        let mut entries = BTreeMap::new();
        let mut partial = false;
        while let Some(result) = pending.join_next().await {
            match result {
                Ok((index, (entry, incomplete))) => {
                    entries.insert(index, entry);
                    partial |= incomplete;
                }
                Err(_) => {
                    partial = true;
                }
            }
        }
        (entries.into_values().collect(), partial)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        extract::State,
        http::{HeaderMap, StatusCode, header},
        response::IntoResponse,
        routing::get,
    };
    use serde_json::json;
    use std::{
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicUsize, Ordering},
    };

    #[derive(Clone)]
    struct Mock {
        mode: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
    }
    async fn upstream(State(mock): State<Mock>, headers: HeaderMap) -> impl IntoResponse {
        assert_eq!(
            headers.get("authorization").unwrap(),
            "Bearer synthetic-token"
        );
        assert_eq!(
            headers.get("chatgpt-account-id").unwrap(),
            "synthetic-account-id"
        );
        assert_eq!(headers.get("user-agent").unwrap(), "codex-cli");
        mock.calls.fetch_add(1, Ordering::SeqCst);
        match mock.mode.load(Ordering::SeqCst) {
            0 => (StatusCode::OK, HeaderMap::new(), json!({"account_id":"synthetic-account-id", "rate_limit":{"primary_window":{"used_percent":23,"reset_at":1_800_000_000},"secondary_window":null},"additional_rate_limits":[{"metered_feature":"code","limit_name":"Code","rate_limit":{"primary_window":{"used_percent":44},"secondary_window":{"used_percent":67}}}]}).to_string()),
            1 => (StatusCode::SERVICE_UNAVAILABLE, HeaderMap::new(), "private-upstream-error".into()),
            2 => (StatusCode::OK, HeaderMap::new(), json!({"account_id":"foreign","rate_limit":{}}).to_string()),
            3 => { let mut h = HeaderMap::new(); h.insert(header::LOCATION, "/redirect-target".parse().unwrap()); (StatusCode::FOUND, h, String::new()) },
            _ => (StatusCode::OK, HeaderMap::new(), "x".repeat(MAX_BYTES + 1)),
        }
    }
    fn account() -> Account {
        Account {
            provider: "openai".into(),
            account_reference: "shared".into(),
            enabled: true,
            provisionable: false,
            credential_profiles: 2,
        }
    }
    #[tokio::test]
    async fn quota_collector_qualifies_mock_protocol_and_preserves_stale_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(&path, json!({"tokens":{"access_token":"synthetic-token","account_id":"synthetic-account-id"}}).to_string()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let source = CodexSource {
            provider_id: "openai".into(),
            account_ref: "shared".into(),
            chatgpt_account_id: "synthetic-account-id".into(),
            auth_file: path.clone(),
            snapshot_file: None,
            account_label: None,
        };
        let mock = Mock {
            mode: Arc::new(AtomicUsize::new(0)),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/usage", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/usage", get(upstream))
            .route("/redirect-target", get(upstream))
            .with_state(mock.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut collector = Collector::new();
        collector.endpoint = endpoint;
        let now = 1_750_000_000_000;
        let (first, second) = tokio::join!(
            collector.collect(vec![account()], std::slice::from_ref(&source), now),
            collector.collect(vec![account()], std::slice::from_ref(&source), now)
        );
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1, "single flight");
        assert_eq!(first.0[0].windows.len(), 4);
        assert_eq!(first.0[0].windows[0].used_percent, Some(23.0));
        assert!(matches!(first.0[0].windows[1].state, QuotaState::Unknown));
        assert_eq!(second.0[0].windows[2].used_percent, Some(44.0));
        let observed = first.0[0].windows[0].observation.observed_at.0.clone();
        for mode in 1..=4 {
            mock.mode.store(mode, Ordering::SeqCst);
            let (stale, partial) = collector
                .collect(
                    vec![account()],
                    std::slice::from_ref(&source),
                    now + CACHE_MS * (mode as u64),
                )
                .await;
            assert!(partial);
            assert_eq!(stale[0].windows[0].observation.observed_at.0, observed);
            assert_eq!(stale[0].windows[0].used_percent, Some(23.0));
            assert!(
                !serde_json::to_string(&stale)
                    .unwrap()
                    .contains("private-upstream-error")
            );
        }
        assert_eq!(
            mock.calls.load(Ordering::SeqCst),
            5,
            "redirect must not be followed"
        );
        let mut fresh = Collector::new();
        fresh.endpoint = collector.endpoint.clone();
        mock.mode.store(1, Ordering::SeqCst);
        let (unavailable, partial) = fresh
            .collect(vec![account()], std::slice::from_ref(&source), now)
            .await;
        assert!(partial);
        assert!(matches!(
            unavailable[0].windows[0].state,
            QuotaState::Unknown
        ));
        assert_eq!(unavailable[0].windows[0].used_percent, None);
        let mut mismatch = source.clone();
        mismatch.chatgpt_account_id = "other".into();
        assert!(auth(&mismatch, now).is_none());
        std::fs::write(&path, json!({"tokens":{"access_token":"e30.eyJleHAiOjF9.signature","account_id":"synthetic-account-id"}}).to_string()).unwrap();
        assert!(auth(&source, now).is_none(), "expired OAuth access token");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(auth(&source, now).is_none());
        server.abort();
    }
    #[test]
    fn quota_percent_and_account_validation_reject_malformed_values() {
        let now = 1_750_000_000_000;
        for percent in [json!(-1), json!(101), json!("25"), Value::Null] {
            let body =
                json!({"rate_limit":{"primary_window":{"used_percent":percent}}}).to_string();
            assert!(parse(body.as_bytes(), "account", now).is_none());
        }
        assert!(
            parse(
                br#"{"account_id":"different","rate_limit":{}}"#,
                "account",
                now
            )
            .is_none()
        );
        let value = parse(
            br#"{"rate_limit":{"primary_window":null,"secondary_window":{"used_percent":0}}}"#,
            "account",
            now,
        )
        .unwrap();
        assert!(matches!(value[0].state, QuotaState::Unknown));
        assert_eq!(value[1].used_percent, Some(0.0));
    }
}

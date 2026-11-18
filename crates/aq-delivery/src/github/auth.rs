//! GitHub App authentication: the App JWT (RS256) and installation access
//! tokens limited to one repository, cached until shortly before they expire.
use super::{Auth, GitHubApiError, GitHubClient, GitHubResult};
use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// GitHub protocol: `iat` is backdated to tolerate clock drift.
const JWT_BACKDATE_SECS: i64 = 60;
/// GitHub protocol: `exp` may be at most 10 minutes after the real time; with
/// the backdated `iat` the token is valid for exactly the 10 minutes allowed.
const JWT_LIFETIME_SECS: i64 = 600;
/// Refresh a cached installation token this long before its `expires_at`.
const TOKEN_REFRESH_MARGIN_SECS: i64 = 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppJwtClaims {
    pub iat: i64,
    pub exp: i64,
    /// The App id, as a string.
    pub iss: String,
}

/// App id and private key. The key never leaves this type.
pub struct AppCredentials {
    app_id: u64,
    key: EncodingKey,
}
impl std::fmt::Debug for AppCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppCredentials")
            .field("app_id", &self.app_id)
            .finish_non_exhaustive()
    }
}
impl AppCredentials {
    /// PKCS#1 (`RSA PRIVATE KEY`, as GitHub issues it) or PKCS#8 PEM.
    pub fn from_pem(app_id: u64, pem: &[u8]) -> GitHubResult<Self> {
        if app_id == 0 {
            return Err(GitHubApiError::InvalidInput("app_id must be positive"));
        }
        let key = EncodingKey::from_rsa_pem(pem)
            .map_err(|_| GitHubApiError::Key("private key is not an RSA PEM key"))?;
        Ok(Self { app_id, key })
    }
    pub fn load(app_id: u64, path: &Path) -> GitHubResult<Self> {
        let pem = std::fs::read(path)
            .map_err(|_| GitHubApiError::Key("private key file cannot be read"))?;
        Self::from_pem(app_id, &pem)
    }
    pub fn app_id(&self) -> u64 {
        self.app_id
    }
    pub fn claims_at(&self, now: DateTime<Utc>) -> AppJwtClaims {
        let now = now.timestamp();
        AppJwtClaims {
            iat: now - JWT_BACKDATE_SECS,
            exp: now - JWT_BACKDATE_SECS + JWT_LIFETIME_SECS,
            iss: self.app_id.to_string(),
        }
    }
    /// The App JWT for `now`.
    pub fn jwt_at(&self, now: DateTime<Utc>) -> GitHubResult<String> {
        jsonwebtoken::encode(
            &Header::new(Algorithm::RS256),
            &self.claims_at(now),
            &self.key,
        )
        .map_err(|_| GitHubApiError::Key("cannot sign the App JWT"))
    }
    pub fn jwt(&self) -> GitHubResult<String> {
        self.jwt_at(Utc::now())
    }
}

/// An installation access token. `Debug` never shows the secret.
#[derive(Clone, PartialEq, Eq)]
pub struct InstallationToken {
    token: String,
    pub expires_at: DateTime<Utc>,
    pub installation_id: u64,
}
impl std::fmt::Debug for InstallationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallationToken")
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("installation_id", &self.installation_id)
            .finish()
    }
}
impl InstallationToken {
    /// The secret, for an `Authorization` header or a git HTTPS credential.
    pub fn secret(&self) -> &str {
        &self.token
    }
    fn fresh_at(&self, now: DateTime<Utc>) -> bool {
        now < self.expires_at - Duration::seconds(TOKEN_REFRESH_MARGIN_SECS)
    }
}

#[derive(Deserialize)]
struct AccessTokenResponse {
    token: String,
    expires_at: DateTime<Utc>,
}

/// `POST /app/installations/{id}/access_tokens` limited to one repository.
pub(super) async fn mint(
    client: &GitHubClient,
    credentials: &AppCredentials,
    installation_id: u64,
    repository: &str,
) -> GitHubResult<InstallationToken> {
    let jwt = credentials.jwt()?;
    let body = serde_json::json!({ "repositories": [repository] });
    let response: AccessTokenResponse = client
        .send(
            reqwest::Method::POST,
            &format!("/app/installations/{installation_id}/access_tokens"),
            Auth::App(&jwt),
            Some(&body),
        )
        .await?;
    if response.token.is_empty() {
        return Err(GitHubApiError::Protocol("empty installation token".into()));
    }
    Ok(InstallationToken {
        token: response.token,
        expires_at: response.expires_at,
        installation_id,
    })
}

/// Tokens by (installation, lowercased repository). The lock is held across
/// minting so concurrent callers for one repository mint once.
#[derive(Default)]
pub(super) struct TokenCache {
    tokens: tokio::sync::Mutex<HashMap<(u64, String), InstallationToken>>,
}
impl TokenCache {
    pub(super) async fn get_or_mint<F, Fut>(
        &self,
        installation_id: u64,
        repository: &str,
        mint: F,
    ) -> GitHubResult<InstallationToken>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = GitHubResult<InstallationToken>>,
    {
        let key = (installation_id, repository.to_ascii_lowercase());
        let mut tokens = self.tokens.lock().await;
        if let Some(token) = tokens.get(&key)
            && token.fresh_at(Utc::now())
        {
            return Ok(token.clone());
        }
        tokens.remove(&key);
        let token = mint().await?;
        tokens.insert(key, token.clone());
        Ok(token)
    }
}

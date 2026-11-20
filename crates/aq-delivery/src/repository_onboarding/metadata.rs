use super::{
    AdoptRequest, Adoption, AdoptionError, CredentialKind, GithubRepository, OnboardingConfig,
};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

pub struct Github<'a> {
    pub config: &'a OnboardingConfig,
    pub token: Option<String>,
}

pub enum Meta {
    Branch { branch: String, signing: bool },
    Unavailable(String),
}

#[derive(Debug, thiserror::Error)]
enum MetadataError {
    #[error("github reported a different repository identity")]
    Identity,
    #[error("github reported an invalid branch")]
    Branch,
    #[error("{0}")]
    Read(String),
}

#[derive(Deserialize)]
struct Rule {
    #[serde(rename = "type")]
    kind: String,
}

enum Client {
    Rest {
        client: reqwest::Client,
        base: String,
        token: Option<String>,
    },
    #[cfg(feature = "delivery_playbook")]
    App {
        client: crate::github::GitHubClient,
        token: crate::github::InstallationToken,
    },
}

impl Client {
    async fn get(&self, path: &str) -> Result<Value, String> {
        match self {
            Self::Rest {
                client,
                base,
                token,
            } => {
                let mut request = client
                    .get(format!("{base}{path}"))
                    .header("accept", "application/vnd.github+json")
                    .header("user-agent", "aq-server-onboarding")
                    .header("x-github-api-version", "2022-11-28");
                if let Some(token) = token {
                    request = request.bearer_auth(token);
                }
                let response = request
                    .send()
                    .await
                    .map_err(|_| "github metadata request failed")?;
                if !response.status().is_success() {
                    return Err(format!(
                        "github metadata returned {}",
                        response.status().as_u16()
                    ));
                }
                response
                    .json()
                    .await
                    .map_err(|_| "github metadata is not JSON".into())
            }
            #[cfg(feature = "delivery_playbook")]
            Self::App { client, token } => client
                .send(
                    reqwest::Method::GET,
                    path,
                    crate::github::Auth::Installation(token),
                    None,
                )
                .await
                .map_err(|e| e.code().into()),
        }
    }

    async fn metadata(
        &self,
        repo: &GithubRepository,
        supplied_branch: Option<&str>,
        verify_identity: bool,
    ) -> Result<(String, bool), MetadataError> {
        let name = format!("{}/{}", repo.owner, repo.name);
        let base = format!("/repos/{name}");
        let branch = match supplied_branch {
            Some(b) => b.to_string(),
            None => {
                let value = self.get(&base).await.map_err(MetadataError::Read)?;
                if verify_identity
                    && !value["full_name"]
                        .as_str()
                        .is_some_and(|n| n.eq_ignore_ascii_case(&name))
                {
                    return Err(MetadataError::Identity);
                }
                value["default_branch"]
                    .as_str()
                    .ok_or(MetadataError::Branch)?
                    .to_string()
            }
        };
        if super::branch_ref(&branch).is_none() {
            return Err(MetadataError::Branch);
        }
        // The branch is one API path segment, including when its name has '/'.
        let mut path = reqwest::Url::parse(&format!("https://api.github.com{base}/rules/branches"))
            .map_err(|_| MetadataError::Branch)?;
        path.path_segments_mut()
            .map_err(|_| MetadataError::Branch)?
            .push(&branch);
        let value = self.get(path.path()).await.map_err(MetadataError::Read)?;
        let rules: Vec<Rule> = serde_json::from_value(value)
            .map_err(|_| MetadataError::Read("github branch rules are malformed".into()))?;
        Ok((
            branch,
            rules.iter().any(|rule| rule.kind == "required_signatures"),
        ))
    }
}

impl Github<'_> {
    fn client(self) -> Result<Client, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "github client unavailable")?;
        Ok(Client::Rest {
            client,
            base: self.config.github_api_url.trim_end_matches('/').into(),
            token: self.token,
        })
    }

    pub async fn metadata(self, repo: &GithubRepository, branch: Option<&str>) -> Meta {
        let result = match self.client() {
            Ok(client) => client.metadata(repo, branch, false).await,
            Err(e) => Err(MetadataError::Read(e)),
        };
        match result {
            Ok((branch, signing)) => Meta::Branch { branch, signing },
            Err(e) => Meta::Unavailable(e.to_string()),
        }
    }
}

impl Adoption<'_> {
    pub(super) async fn default_branch(
        &self,
        request: &AdoptRequest,
    ) -> Result<String, AdoptionError> {
        use AdoptionError::{Conflict, Invalid, Unavailable};
        let fallback = OnboardingConfig::default();
        let config = self.onboarding.unwrap_or(&fallback);
        let client = if let Some(name) = &request.credential_profile {
            let profile = config
                .credential_profiles
                .get(name)
                .filter(|p| super::label(name) && p.kind == CredentialKind::HttpsFineGrainedToken)
                .ok_or(Invalid("unknown GitHub metadata credential profile"))?;
            let token = tokio::fs::read_to_string(&profile.secret_file)
                .await
                .map_err(|_| Unavailable("GitHub metadata credential"))?;
            Github {
                config,
                token: Some(token.trim().to_string()),
            }
            .client()
            .map_err(|_| Unavailable("GitHub metadata"))?
        } else {
            self.github_client(config, &request.github).await?
        };
        let (branch, signing) =
            client
                .metadata(&request.github, None, true)
                .await
                .map_err(|e| match e {
                    MetadataError::Identity => Conflict("GitHub repository identity differs"),
                    MetadataError::Branch => Conflict("GitHub default branch is invalid"),
                    MetadataError::Read(_) => Unavailable("GitHub metadata"),
                })?;
        if request
            .default_branch
            .as_deref()
            .is_some_and(|expected| expected != branch)
        {
            return Err(Conflict(
                "GitHub default branch differs from the expected one",
            ));
        }
        if signing {
            return Err(Conflict("GitHub default branch requires signed commits"));
        }
        Ok(branch)
    }

    async fn github_client(
        &self,
        config: &OnboardingConfig,
        repo: &GithubRepository,
    ) -> Result<Client, AdoptionError> {
        #[cfg(feature = "delivery_playbook")]
        if let Some(settings) = self.github_app.filter(|app| {
            app.installations
                .iter()
                .any(|i| i.owner.eq_ignore_ascii_case(&repo.owner))
        }) {
            let app = crate::github::GitHubApp::from_config(Some(settings))
                .map_err(|_| AdoptionError::Unavailable("GitHub App credentials"))?;
            let token = app
                .installation_token(&repo.owner, &repo.name)
                .await
                .map_err(|_| AdoptionError::Unavailable("GitHub App installation token"))?;
            return Ok(Client::App {
                client: app.client().clone(),
                token,
            });
        }
        #[cfg(not(feature = "delivery_playbook"))]
        let _ = repo;
        Github {
            config,
            token: None,
        }
        .client()
        .map_err(|_| AdoptionError::Unavailable("GitHub metadata"))
    }
}

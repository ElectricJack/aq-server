//! Repository onboarding requests, external observations and persisted receipts.
mod adoption;
mod metadata;

pub use adoption::{AdoptRequest, Adoption, AdoptionError};
pub use metadata::{Github, Meta};

use crate::{PublicationMode, WaitForChecks};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, PgPool, Row as _};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use uuid::Uuid;

/// Vault `[onboarding]`. The API base and every credential are server-held.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnboardingConfig {
    #[serde(default = "default_api")]
    pub github_api_url: String,
    #[serde(default)]
    pub credential_profiles: BTreeMap<String, CredentialProfile>,
}
fn default_api() -> String {
    "https://api.github.com".into()
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialProfile {
    pub kind: CredentialKind,
    /// Vault file holding the deploy key or fine-grained token. Only its
    /// profile name ever leaves the vault; aq-git selects it by that name.
    pub secret_file: PathBuf,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    SshDeployKey,
    HttpsFineGrainedToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Ssh,
    Https,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubRepository {
    pub owner: String,
    pub name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationRequest {
    pub mode: PublicationMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_for_checks: Option<WaitForChecks>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnboardRequest {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    pub repository_id: Uuid,
    pub github: GithubRepository,
    pub transport: Transport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_profile: Option<String>,
    /// Owner override; otherwise GitHub's `default_branch` is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    pub publication: PublicationRequest,
    /// Project binding fields other than the server-filled repository fields.
    pub template: Map<String, Value>,
}

impl Default for OnboardingConfig {
    fn default() -> Self {
        Self {
            github_api_url: default_api(),
            credential_profiles: BTreeMap::new(),
        }
    }
}

pub fn slug(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 100
        && v != "."
        && v != ".."
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}
pub fn label(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 128
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.@:".contains(&b))
}

pub enum Git {
    Ok(Value),
    Refused { code: String, message: String },
    Unavailable,
}
pub async fn aq_git(
    socket: Option<&Path>,
    context: crate::OperationContext,
    operation: &str,
    params: Value,
) -> Git {
    let Some(socket) = socket else {
        return Git::Unavailable;
    };
    let request_id = context.operation_id.to_string();
    let request = json!({
        "protocol_version":1,"request_id":request_id,"operation":operation,"repository_id":context.repository_id,
        "operation_id":context.operation_id,"expected_binding_generation":context.binding_generation,"params":params,"deadline_ms":30000
    });
    let mut bytes = match serde_json::to_vec(&request) {
        Ok(bytes) => bytes,
        Err(_) => return Git::Unavailable,
    };
    bytes.push(b'\n');
    let v = match crate::protocol::exchange(socket, &bytes).await {
        Ok(raw) => serde_json::from_slice::<Value>(&raw).ok(),
        Err(_) => None,
    };
    git_reply(v, &request_id)
}

fn git_reply(v: Option<Value>, request_id: &str) -> Git {
    match v.filter(|v| v["protocol_version"] == 1 && v["request_id"] == request_id) {
        Some(v) if v["ok"] == true && v["result"].is_object() => Git::Ok(v["result"].clone()),
        Some(v) if v["ok"] == false => match v["error"]["code"].as_str() {
            Some(code) => Git::Refused {
                code: code.into(),
                message: v["error"]["message"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(512)
                    .collect(),
            },
            None => Git::Unavailable,
        },
        _ => Git::Unavailable,
    }
}

pub struct Row {
    pub actor: Uuid,
    pub digest: String,
    pub step: String,
    pub state: Value,
}
pub async fn load(conn: &mut PgConnection, operation_id: Uuid) -> Result<Option<Row>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT actor_id,request_digest,step,state FROM repository_onboardings WHERE operation_id=$1",
    )
    .bind(operation_id)
    .fetch_optional(conn).await?;
    Ok(row.map(|r| Row {
        actor: r.get(0),
        digest: r.get(1),
        step: r.get(2),
        state: r.get(3),
    }))
}

/// The actor and canonical digest bound to an idempotent onboarding operation.
#[derive(Clone, Copy)]
pub struct RequestOwner<'a> {
    pub actor: Uuid,
    pub digest: &'a str,
}

/// The persisted state and step are advanced together.
pub struct Progress<'a> {
    pub step: &'a str,
    pub state: &'a Value,
}

pub async fn save(
    pool: &PgPool,
    operation_id: Uuid,
    progress: Progress<'_>,
    now_ms: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE repository_onboardings SET step=$2,state=$3,updated_at_ms=$4 WHERE operation_id=$1 AND step NOT IN ('bound','refused')",
    )
    .bind(operation_id)
    .bind(progress.step)
    .bind(progress.state)
    .bind(now_ms)
    .execute(pool).await?;
    Ok(())
}
pub fn response(row: &Row) -> Value {
    let s = &row.state;
    let disposition = match row.step.as_str() {
        "bound" => "bound",
        "refused" => "refused",
        _ => "incomplete",
    };
    let mut out = json!({"operation_id":s["request"]["operation_id"],"step":row.step,"disposition":disposition,
        "project_id":s["request"]["project_id"],"repository_id":s["request"]["repository_id"],
        "remote_url":s["remote_url"],"credential_profile":s["request"]["credential_profile"],
        "default_branch":s["default_branch"],"repository_binding_generation":s["binding_generation"],"base":s["base"]});
    for key in [
        "refusal",
        "error",
        "project",
        "adopted",
        "default_ref",
        "repository_identity",
        "repository_observation_id",
    ] {
        if !s[key].is_null() {
            out[key] = s[key].clone();
        }
    }
    out
}

fn branch_ref(branch: &str) -> Option<String> {
    let target = format!("refs/heads/{branch}");
    (target.len() <= 1024 && crate::branch(&target).is_ok()).then_some(target)
}

pub struct NewOnboarding<'a> {
    pub request: &'a OnboardRequest,
    pub remote_url: String,
}

pub async fn start(
    conn: &mut PgConnection,
    owner: RequestOwner<'_>,
    onboarding: NewOnboarding<'_>,
    now_ms: i64,
) -> Result<(), sqlx::Error> {
    let NewOnboarding {
        request,
        remote_url,
    } = onboarding;
    sqlx::query(
        "INSERT INTO repository_onboardings \
         (operation_id,actor_id,request_digest,project_id,repository_id,step,state,created_at_ms,updated_at_ms) \
         VALUES ($1,$2,$3,$4,$5,'started',$6,$7,$7) ON CONFLICT(operation_id) DO NOTHING",
    )
    .bind(request.operation_id)
    .bind(owner.actor)
    .bind(owner.digest)
    .bind(request.project_id)
    .bind(request.repository_id)
    .bind(json!({"request":request,"remote_url":remote_url}))
    .bind(now_ms)
    .execute(conn)
    .await?;
    Ok(())
}

#[cfg(feature = "delivery_playbook")]
pub(crate) async fn delivery_state(
    pool: &PgPool,
    project: Uuid,
    repository: Uuid,
    generation: u64,
) -> Result<Option<Value>, sqlx::Error> {
    let Ok(generation) = i64::try_from(generation) else {
        return Ok(None);
    };
    // Missing or malformed generations are ambiguous, even beside a known
    // row. A positively identified older generation can never win.
    let rows: Vec<(Value, Option<String>)> = sqlx::query_as(
        "SELECT o.state, b.binding#>>'{publication,target}' \
         FROM repository_onboardings o JOIN project_execution_bindings b USING (project_id) \
         WHERE o.project_id=$1 AND o.repository_id=$2 AND o.step='bound' \
           AND b.binding->'enabled'='true'::jsonb \
           AND b.binding->'repository_id'=to_jsonb($2::uuid) \
           AND b.binding->'repository_binding_generation'=to_jsonb($3::bigint) AND (\
           state->'binding_generation'=to_jsonb($3::bigint) OR \
           jsonb_typeof(state->'binding_generation') IS DISTINCT FROM 'number' OR \
           (state->>'binding_generation') !~ '^[1-9][0-9]*$') LIMIT 2",
    )
    .bind(project)
    .bind(repository)
    .bind(generation)
    .fetch_all(pool)
    .await?;
    let [(state, publication)] = rows.as_slice() else {
        return Ok(None);
    };
    if state["binding_generation"].as_i64() != Some(generation) {
        return Ok(None);
    }
    let target = state["default_branch"].as_str().and_then(branch_ref);
    if target.as_deref().is_none_or(|target| {
        publication.as_deref().is_some_and(|p| p != target)
            || (state["adopted"] == true && state["default_ref"] != target)
    }) {
        return Ok(None);
    }
    Ok(Some(state.clone()))
}

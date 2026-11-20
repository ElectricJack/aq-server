//! Recompile retained routing inputs across the Host9/Host10 catalog boundary.
use crate::{Config, Error, Host, Inputs, Result, Saved, Server, builtin, request};
use aq_playbooks_model::Artifact;
use aq_playbooks_simulation::cases::{Case, CaseReturn};
use serde_json::{Map, Value};
use sqlx::{Row, types::Json};
use uuid::Uuid;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Recompile {
    from_artifact: String,
    to_artifact: String,
}

pub struct Target {
    namespace_id: Uuid,
    generation: i64,
    installation_routing: bool,
    no_result: Value,
}

impl Target {
    pub fn new(
        namespace_id: Uuid,
        generation: i64,
        installation_routing: bool,
        no_result: Value,
    ) -> Self {
        Self {
            namespace_id,
            generation,
            installation_routing,
            no_result,
        }
    }
}

struct Retained {
    id: i64,
    source_revision: i64,
    source_digest: String,
    facts_digest: String,
    record_digest: String,
    artifact: Json<Artifact>,
    markdown: String,
    case_files: Json<Vec<Case>>,
    draft: Json<Value>,
    facts: String,
}

/// Verify the old seal and all cases against both catalogs. The only fixture
/// change allowed is adding the Host10 no-result fields to observed reads.
fn recompile(
    retained: &Retained,
    mut inputs: Inputs,
    no_result: &Map<String, Value>,
    config: &Config,
) -> Result<(Saved, Vec<Case>)> {
    let check = |inputs: &Inputs, cases: &[Case]| {
        crate::check_saved(&retained.draft.0, cases, inputs, config)
            .map_err(|e| Error::Conflict(format!("Routing fixture check failed: {}", e.owner)))
    };
    let original = check(&inputs, &retained.case_files.0)?;
    if original.artifact != retained.artifact.0 {
        return Err(Error::Conflict(
            "The retained inputs did not reproduce the old routing artifact".into(),
        ));
    }
    let mut cases = retained.case_files.0.clone();
    for case in &mut cases {
        let Some(responses) = case.returns.get_mut("work.read") else {
            continue;
        };
        for response in responses {
            if let CaseReturn::Answer { outcome, output } = response
                && outcome == "observed"
            {
                let facts = output["facts"].as_object_mut().ok_or(Error::Corrupt)?;
                if no_result.keys().any(|key| facts.contains_key(key)) {
                    return Err(Error::Corrupt);
                }
                facts.extend(no_result.clone());
            }
        }
    }
    inputs.catalog = request::catalog_2_3()?;
    let candidate = check(&inputs, &cases)?;
    if original.cases != candidate.cases
        || original.artifact.content.playbook.required_capabilities
            != candidate.artifact.content.playbook.required_capabilities
    {
        return Err(Error::Conflict(
            "Routing results or capabilities changed across the catalog boundary".into(),
        ));
    }
    Ok((candidate, cases))
}

/// Start from the retained approved run and record the resulting checked save
/// so the ordinary deploy/retry path can activate it with the source's grant.
pub(crate) async fn activate<S: Server>(
    s: &S,
    owner: Uuid,
    target: Target,
    request: Recompile,
) -> Result<()> {
    let pool = s.as_ref();
    if !target.installation_routing {
        return Err(Error::Conflict(
            "Routing continuity requires installation_routing".into(),
        ));
    }
    let Recompile {
        from_artifact: from,
        to_artifact: to,
    } = request;
    let is_digest = |digest: &str| {
        digest.strip_prefix("sha256:").is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
    };
    if from == to || !is_digest(&from) || !is_digest(&to) {
        return Err(Error::Invalid(
            "Two different artifact digests are required".into(),
        ));
    }
    let enabled: bool = sqlx::query_scalar("SELECT enabled FROM routing_state WHERE singleton")
        .fetch_one(pool)
        .await?;
    if enabled {
        return Err(Error::Conflict(
            "Turn automatic routing off before changing its catalog".into(),
        ));
    }
    let capabilities = builtin::routing().capabilities;
    let approved: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM playbook_namespaces n
           JOIN playbook_engine_bindings b USING(namespace_id,generation)
           JOIN playbook_grants g USING(namespace_id,generation)
           JOIN playbook_namespaces target ON target.namespace_id=$1 AND target.generation=$2
           WHERE target.catalog_profile=10 AND target.owner_user_id=$3
             AND n.catalog_profile=9 AND n.workspace_id=target.workspace_id
             AND n.project_id=target.project_id AND n.owner_user_id=$3
             AND b.binding=$4 AND b.artifact=$5 AND g.owner_user_id=$3
             AND g.scope=jsonb_build_object('workspace_id',n.workspace_id,'project_id',n.project_id,
                 'capabilities',$6::jsonb,'recipients','[]'::jsonb)
             AND g.expires_at_ms=$7 AND g.revoked_at_ms IS NULL)",
    )
    .bind(target.namespace_id)
    .bind(target.generation)
    .bind(owner)
    .bind(builtin::ROUTING_BINDING)
    .bind(&from)
    .bind(Json(capabilities))
    .bind(crate::SOURCE_GRANT_EXPIRES_AT_MS)
    .fetch_one(pool)
    .await?;
    if !approved {
        return Err(Error::Conflict(
            "The referenced Host9 binding and source grant are unavailable".into(),
        ));
    }
    let source = builtin::ROUTING;
    let record = crate::get(pool, source)
        .await?
        .ok_or_else(|| Error::NotFound("The routing source is missing".into()))?;
    let current = crate::active(pool, source).await?.ok_or_else(no_source)?;
    if current.artifact_digest == to && current.previous_digest.as_deref() == Some(from.as_str()) {
        crate::deploy(s, current.id).await?;
        return acknowledged(s, &target, &to).await;
    }
    if current.artifact_digest != from {
        return Err(no_source());
    }
    let retained = sqlx::query(
        "SELECT r.id,r.source_revision,r.source_digest,r.facts_digest,r.record_digest,
             r.artifact,r.markdown,r.case_files,t.answer->'draft' AS draft,
             t.request->'agent'->'files'->>'facts.json' AS facts
         FROM playbook_authoring_runs r JOIN internal_tasks t ON t.id=r.internal_task_id
         WHERE r.id=$1 AND r.active AND r.status='saved' AND r.markdown IS NOT NULL
           AND r.case_files IS NOT NULL AND t.answer->>'artifact'=r.artifact_digest
           AND t.answer->'cases'=r.case_files AND t.answer->>'facts_digest'=r.facts_digest
           AND t.answer->'draft' IS NOT NULL
           AND t.request->'agent'->'files'->>'facts.json' IS NOT NULL",
    )
    .bind(current.id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| {
        Error::Conflict("The original routing draft, facts or fixtures are unavailable".into())
    })?;
    let retained = Retained {
        id: retained.try_get("id")?,
        source_revision: retained.try_get("source_revision")?,
        source_digest: retained.try_get("source_digest")?,
        facts_digest: retained.try_get("facts_digest")?,
        record_digest: retained.try_get("record_digest")?,
        artifact: retained.try_get("artifact")?,
        markdown: retained.try_get("markdown")?,
        case_files: retained.try_get("case_files")?,
        draft: retained.try_get("draft")?,
        facts: retained.try_get("facts")?,
    };
    let (markdown, _) = Host::markdown(s, &record).await?;
    let facts = serde_json::from_str(&retained.facts).map_err(|_| Error::Corrupt)?;
    if markdown != retained.markdown
        || crate::source_digest(&markdown) != retained.source_digest
        || crate::record_digest(&record) != retained.record_digest
        || crate::facts_digest(&facts) != retained.facts_digest
    {
        return Err(Error::Conflict(
            "The saved routing inputs no longer match their source or recorded digests".into(),
        ));
    }
    let inputs = Inputs {
        grant_capabilities: record.capabilities.clone(),
        catalog: request::pinned_catalog(&record)?,
        record,
        markdown,
        facts,
        facts_version: retained.facts_digest.clone(),
        previous: None,
    };
    let config = s.config();
    let deadline = std::time::Duration::from_millis(config.check_deadline_ms);
    let no_result = target.no_result.as_object().ok_or(Error::Corrupt)?.clone();
    let check = tokio::task::spawn_blocking(move || {
        let result = recompile(&retained, inputs, &no_result, &config);
        (retained, result)
    });
    let (retained, result) = tokio::time::timeout(deadline, check)
        .await
        .map_err(|_| Error::Conflict("Routing fixture check timed out".into()))?
        .map_err(|_| Error::Corrupt)?;
    let (candidate, cases) = result?;
    if candidate.artifact.digest != to {
        return Err(Error::Conflict(
            "The checked routing artifact did not match the expected Host10 digest".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM playbook_sources WHERE id=$1 AND deleted_at_ms IS NULL FOR UPDATE")
        .bind(source)
        .fetch_one(&mut *tx)
        .await?;
    let is_current: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM playbook_authoring_runs WHERE id=$1 AND active)
         AND NOT EXISTS(SELECT 1 FROM playbook_authoring_runs WHERE source_id=$2 AND status='running')",
    )
    .bind(retained.id)
    .bind(source)
    .fetch_one(&mut *tx)
    .await?;
    if !is_current {
        return Err(no_source());
    }
    sqlx::query("UPDATE playbook_authoring_runs SET active=FALSE WHERE id=$1")
        .bind(retained.id)
        .execute(&mut *tx)
        .await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO playbook_authoring_runs(source_id,source_revision,source_digest,facts_digest,
             record_digest,status,active,artifact_digest,artifact,previous_digest,cases,
             markdown,case_files,created_at_ms,finished_at_ms,activated_at_ms,deployed)
         VALUES($1,$2,$3,$4,$5,'saved',TRUE,$6,$7,$8,$9,$10,$11,$12,$12,$12,FALSE) RETURNING id",
    )
    .bind(source)
    .bind(retained.source_revision)
    .bind(&retained.source_digest)
    .bind(&retained.facts_digest)
    .bind(&retained.record_digest)
    .bind(&to)
    .bind(Json(candidate.artifact))
    .bind(&from)
    .bind(Json(candidate.cases))
    .bind(&retained.markdown)
    .bind(Json(cases))
    .bind(s.now_ms())
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    crate::deploy(s, id).await?;
    acknowledged(s, &target, &to).await
}

async fn acknowledged<S: Server>(s: &S, target: &Target, digest: &str) -> Result<()> {
    let active: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM playbook_engine_bindings
         WHERE namespace_id=$1 AND generation=$2 AND binding=$3 AND artifact=$4)",
    )
    .bind(target.namespace_id)
    .bind(target.generation)
    .bind(builtin::ROUTING_BINDING)
    .bind(digest)
    .fetch_one(s.as_ref())
    .await?;
    if !active {
        return Err(Error::Conflict(
            "The Host10 routing activation has not been acknowledged".into(),
        ));
    }
    Ok(())
}

fn no_source() -> Error {
    Error::Conflict(
        "The expected routing artifact is no longer the active save, or authoring is in flight"
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::{docs, draft};
    use serde_json::json;

    #[test]
    fn recompile_reproduces_old_seal_and_preserves_routing_cases() {
        let files = docs::example_files();
        let example = |path: &str| {
            let name = format!("escalate-on-judgment/{path}");
            files.iter().find(|(file, _)| *file == name).unwrap().1
        };
        let record = builtin::routing();
        let inputs = Inputs {
            grant_capabilities: record.capabilities.clone(),
            record,
            markdown: example("source.md").into(),
            catalog: request::pinned_catalog(&builtin::routing()).unwrap(),
            facts: serde_json::from_str(example("facts.json")).unwrap(),
            facts_version: "settings-7".into(),
            previous: None,
        };
        let draft_json = serde_json::from_str(example("draft.json")).unwrap();
        let cases = [
            "low-risk.json",
            "high-risk.json",
            "owner-chose.json",
            "task-gone.json",
            "judgment-fails.json",
        ]
        .into_iter()
        .map(|name| serde_json::from_str(example(&format!("cases/{name}"))).unwrap())
        .collect::<Vec<_>>();
        let config = Config::default();
        let baseline = crate::check_saved(&draft_json, &cases, &inputs, &config).unwrap();
        let retained = Retained {
            id: 0,
            source_revision: 0,
            source_digest: String::new(),
            facts_digest: String::new(),
            record_digest: String::new(),
            artifact: Json(baseline.artifact.clone()),
            markdown: inputs.markdown.clone(),
            case_files: Json(cases),
            draft: Json(draft_json),
            facts: String::new(),
        };
        let no_result = json!({
            "outcome": "", "changed": false, "branch": "", "head": "", "base": "",
            "main_head": "", "returns": 0, "pull_request": null
        });
        let no_result = no_result.as_object().unwrap();
        let (next, cases) = recompile(&retained, inputs.clone(), no_result, &config).unwrap();
        assert_eq!(baseline.cases, next.cases);
        assert_ne!(baseline.artifact.digest, next.artifact.digest);
        assert_eq!(
            draft::unfill(&baseline.artifact),
            draft::unfill(&next.artifact)
        );
        assert_eq!(
            next.artifact.content.registry.commands["work.read"].version,
            "2.3.0"
        );
        assert_eq!(cases[3], retained.case_files.0[3]);
        assert!(recompile(&retained, inputs, &Map::new(), &config).is_err());
    }
}

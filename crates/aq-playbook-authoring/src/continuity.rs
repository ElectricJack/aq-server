//! Recompile retained routing inputs across the Host9/Host10 catalog boundary.
use crate::{Config, Error, Host, Inputs, Result, Saved, Server, builtin, request};
use aq_playbooks_model::Artifact;
use aq_playbooks_simulation::cases::{Case, CaseReturn};
use serde_json::{Map, Value};
use sqlx::types::Json;
mod storage;
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
        add_no_result(case, no_result)?;
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

fn add_no_result(case: &mut Case, no_result: &Map<String, Value>) -> Result<()> {
    let Some(responses) = case.returns.get_mut("work.read") else {
        return Ok(());
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
    Ok(())
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
    request.validate(&target)?;
    storage::ensure_approved(pool, owner, &target, &request.from_artifact).await?;
    let source = builtin::ROUTING;
    let record = crate::get(pool, source)
        .await?
        .ok_or_else(|| Error::NotFound("The routing source is missing".into()))?;
    let current = crate::active(pool, source).await?.ok_or_else(no_source)?;
    if current.artifact_digest == request.to_artifact
        && current.previous_digest.as_deref() == Some(request.from_artifact.as_str())
    {
        crate::deploy(s, current.id).await?;
        return acknowledged(s, &target, &request.to_artifact).await;
    }
    if current.artifact_digest != request.from_artifact {
        return Err(no_source());
    }
    let retained = storage::load(pool, current.id).await?;
    let inputs = retained.inputs(s, record).await?;
    let checked = Checked::new(s.config(), &target.no_result, retained, inputs).await?;
    if checked.candidate.artifact.digest != request.to_artifact {
        return Err(Error::Conflict(
            "The checked routing artifact did not match the expected Host10 digest".into(),
        ));
    }
    let id = request.record(s, checked).await?;
    crate::deploy(s, id).await?;
    acknowledged(s, &target, &request.to_artifact).await
}

struct Checked {
    retained: Retained,
    candidate: Saved,
    cases: Vec<Case>,
}

impl Recompile {
    fn validate(&self, target: &Target) -> Result<()> {
        if !target.installation_routing {
            return Err(Error::Conflict(
                "Routing continuity requires installation_routing".into(),
            ));
        }
        let is_digest = |digest: &str| {
            digest.strip_prefix("sha256:").is_some_and(|hex| {
                hex.len() == 64
                    && hex
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            })
        };
        let digests = [&self.from_artifact, &self.to_artifact];
        if self.from_artifact == self.to_artifact
            || !digests.into_iter().all(|digest| is_digest(digest))
        {
            return Err(Error::Invalid(
                "Two different artifact digests are required".into(),
            ));
        }
        Ok(())
    }
}

impl Checked {
    async fn new(
        config: Config,
        no_result: &Value,
        retained: Retained,
        inputs: Inputs,
    ) -> Result<Checked> {
        let deadline = std::time::Duration::from_millis(config.check_deadline_ms);
        let no_result = no_result.as_object().ok_or(Error::Corrupt)?.clone();
        let check = tokio::task::spawn_blocking(move || {
            let result = recompile(&retained, inputs, &no_result, &config);
            (retained, result)
        });
        let (retained, result) = tokio::time::timeout(deadline, check)
            .await
            .map_err(|_| Error::Conflict("Routing fixture check timed out".into()))?
            .map_err(|_| Error::Corrupt)?;
        let (candidate, cases) = result?;
        Ok(Checked {
            retained,
            candidate,
            cases,
        })
    }
}

impl Retained {
    async fn inputs<S: Server>(&self, s: &S, record: crate::SourceRecord) -> Result<Inputs> {
        let (markdown, _) = Host::markdown(s, &record).await?;
        let facts = serde_json::from_str(&self.facts).map_err(|_| Error::Corrupt)?;
        let source_matches =
            markdown == self.markdown && crate::source_digest(&markdown) == self.source_digest;
        let digests_match = crate::record_digest(&record) == self.record_digest
            && crate::facts_digest(&facts) == self.facts_digest;
        if !(source_matches && digests_match) {
            return Err(Error::Conflict(
                "The saved routing inputs no longer match their source or recorded digests".into(),
            ));
        }
        Ok(Inputs {
            grant_capabilities: record.capabilities.clone(),
            // The 2.2.0 catalog the retained Host9 artifact was sealed against.
            catalog: crate::compiler::docs::catalog_registry(),
            record,
            markdown,
            facts,
            facts_version: self.facts_digest.clone(),
            previous: None,
            previous_cases: vec![],
        })
    }
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
            catalog: docs::catalog_registry(),
            facts: serde_json::from_str(example("facts.json")).unwrap(),
            facts_version: "settings-7".into(),
            previous: None,
            previous_cases: vec![],
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

//! Retained definitions, admissions, and decisions used by playbook inspection.
use crate::{
    Error, Namespace, Result,
    catalog_view::Activation,
    inspection::{AdmissionView, RunView},
    playbook_view::{ActivationPin, WaitingView, definition_view, graph_for_storage},
};
use aq_playbook_host::model::Artifact;
use sqlx::{PgConnection, Row};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub struct ViewStore<'a>(pub &'a Namespace);

impl ViewStore<'_> {
    /// Idempotent: the first install of an artifact in this namespace
    /// generation fixes its per-name version.
    pub async fn register_definition(
        &self,
        db: &mut PgConnection,
        artifact: &Artifact,
        now: i64,
    ) -> Result<()> {
        let graph = graph_for_storage(&artifact.content.playbook)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(format!(
                "aq.playbook.definitions/{}/{}",
                self.0.namespace_id, self.0.generation
            ))
            .execute(&mut *db)
            .await?;
        sqlx::query("INSERT INTO playbook_definitions(namespace_id,generation,artifact,name,version,graph,registered_at_ms) SELECT $1,$2,$3,$4,COALESCE(MAX(version),0)+1,$5,$6 FROM playbook_definitions WHERE namespace_id=$1 AND generation=$2 AND name=$4 ON CONFLICT (namespace_id,generation,artifact) DO NOTHING")
            .bind(self.0.namespace_id)
            .bind(self.0.generation)
            .bind(&artifact.digest)
            .bind(&artifact.content.playbook.id)
            .bind(graph)
            .bind(now)
            .execute(&mut *db)
            .await?;
        Ok(())
    }
    pub async fn registered(
        &self,
        db: &mut PgConnection,
        artifacts: &[String],
    ) -> Result<BTreeMap<String, (String, i64)>> {
        let rows = sqlx::query("SELECT artifact,name,version FROM playbook_definitions WHERE namespace_id=$1 AND generation=$2 AND artifact=ANY($3)")
            .bind(self.0.namespace_id)
            .bind(self.0.generation)
            .bind(artifacts)
            .fetch_all(&mut *db)
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| (row.get("artifact"), (row.get("name"), row.get("version"))))
            .collect())
    }
    /// Binding and activation revision of runs admitted through a retained
    /// admission. Binding is null for admissions prepared before it was kept.
    pub async fn admitted_runs(
        &self,
        db: &mut PgConnection,
        runs: &[String],
    ) -> Result<BTreeMap<String, (Option<String>, i64)>> {
        if runs.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows = sqlx::query("SELECT receipt->>'run_id' AS run_id,binding,expected_activation_revision FROM playbook_admissions WHERE namespace_id=$1 AND generation=$2 AND receipt IS NOT NULL AND receipt->>'run_id'=ANY($3)")
            .bind(self.0.namespace_id)
            .bind(self.0.generation)
            .bind(runs)
            .fetch_all(&mut *db)
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    row.get("run_id"),
                    (row.get("binding"), row.get("expected_activation_revision")),
                )
            })
            .collect())
    }
    pub async fn pending_decisions(
        &self,
        db: &mut PgConnection,
        intents: &[String],
    ) -> Result<BTreeMap<String, WaitingView>> {
        if intents.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows = sqlx::query("SELECT e.intent_id,e.receipt->>'handle' AS handle,d.state,s.task_id FROM playbook_effects e LEFT JOIN human_decision_requests d ON d.decision_id::text=e.receipt->>'handle' LEFT JOIN review_subjects s ON s.subject_digest=d.subject_digest WHERE e.namespace_id=$1 AND e.generation=$2 AND e.command_name='decision.request' AND e.intent_id=ANY($3)")
            .bind(self.0.namespace_id)
            .bind(self.0.generation)
            .bind(intents)
            .fetch_all(&mut *db)
            .await?;
        let mut out = BTreeMap::new();
        for row in rows {
            let intent: String = row.get("intent_id");
            // A queued or refused request has no decision handle yet: the run
            // is not waiting on a human decision, so no view (never an error
            // that would hide the whole runs page).
            let Some(decision_id) = row
                .get::<Option<String>, _>("handle")
                .and_then(|h| Uuid::parse_str(&h).ok())
            else {
                continue;
            };
            out.insert(
                intent.clone(),
                WaitingView {
                    kind: "decision".into(),
                    intent,
                    decision_id,
                    decision_state: row.get("state"),
                    related_task_ids: row.get::<Option<Uuid>, _>("task_id").into_iter().collect(),
                },
            );
        }
        Ok(out)
    }
    /// Join already authorized run projections to retained definition and decision rows.
    pub async fn enrich_runs(
        &self,
        db: &mut PgConnection,
        runs: &mut [RunView],
        activations: Option<&[Activation]>,
    ) -> Result<()> {
        let artifacts = runs
            .iter()
            .map(|r| r.artifact.clone())
            .chain(
                activations
                    .into_iter()
                    .flatten()
                    .map(|a| a.artifact.clone()),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let registry = self.registered(&mut *db, &artifacts).await?;
        let intents = runs
            .iter()
            .flat_map(|r| r.pending_intents.iter().cloned())
            .collect::<Vec<_>>();
        let waiting = self.pending_decisions(&mut *db, &intents).await?;
        let ids = runs.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
        let admitted = self.admitted_runs(&mut *db, &ids).await?;
        for run in runs.iter_mut() {
            if let Some((binding, revision)) = admitted.get(&run.id) {
                run.binding = binding.clone();
                run.activation_revision = Some(revision.to_string());
            }
            run.waiting = run
                .pending_intents
                .iter()
                .find_map(|intent| waiting.get(intent).cloned());
            run.definition = registry.get(&run.artifact).map(|(name, version)| {
                let pins = activations.map(|items| {
                    items.iter().map(|item| ActivationPin {
                        binding: &item.binding,
                        artifact: &item.artifact,
                        revision: &item.revision,
                    })
                });
                definition_view(
                    &run.artifact,
                    run.binding.as_deref(),
                    name,
                    *version,
                    pins,
                    &registry,
                )
            });
        }
        Ok(())
    }
    pub async fn admissions(
        &self,
        db: &mut PgConnection,
        after: Option<Uuid>,
        limit: usize,
    ) -> Result<(Vec<AdmissionView>, Option<String>)> {
        let rows=sqlx::query("SELECT request_id,status,held_reason,created_at_ms,receipt,engine_key,request_digest,expected_activation_revision FROM playbook_admissions WHERE namespace_id=$1 AND generation=$2 AND ($3::uuid IS NULL OR request_id>$3) ORDER BY request_id LIMIT $4")
            .bind(self.0.namespace_id).bind(self.0.generation).bind(after).bind((limit+1) as i64).fetch_all(&mut *db).await?;
        let more = rows.len() > limit;
        let items = rows
            .into_iter()
            .take(limit)
            .map(admission_view)
            .collect::<Result<Vec<_>>>()?;
        let next_cursor = more
            .then(|| items.last().map(|r| r.request_id.to_string()))
            .flatten();
        Ok((items, next_cursor))
    }
}

fn checked_receipt(
    row: &sqlx::postgres::PgRow,
) -> Result<Option<aq_playbook_host::runner::KeyedAdmissionReceipt>> {
    let receipt: Option<serde_json::Value> = row.get("receipt");
    let receipt = receipt
        .map(serde_json::from_value::<aq_playbook_host::runner::KeyedAdmissionReceipt>)
        .transpose()
        .map_err(|_| Error::Unknown)?;
    if let Some(r) = &receipt {
        let key: &str = row.get("engine_key");
        let digest: &str = row.get("request_digest");
        let revision: i64 = row.get("expected_activation_revision");
        let retained = (key, digest, Some(revision));
        let observed = (
            r.key.as_str(),
            r.request_digest.as_str(),
            i64::try_from(r.activation_revision).ok(),
        );
        if observed != retained {
            return Err(Error::Conflict);
        }
    }
    Ok(receipt)
}

fn admission_view(row: sqlx::postgres::PgRow) -> Result<AdmissionView> {
    let receipt = checked_receipt(&row)?;
    Ok(AdmissionView {
        request_id: row.get("request_id"),
        status: row.get("status"),
        created_at_ms: row.get("created_at_ms"),
        run_id: receipt.as_ref().map(|r| r.run_id.clone()),
        activation_revision: receipt.as_ref().map(|r| r.activation_revision.to_string()),
        catalog_revision: receipt.as_ref().map(|r| r.catalog_revision.to_string()),
        held_reason: row.get("held_reason"),
    })
}

use super::{Checked, Recompile, Retained, Target, no_source};
use crate::{Error, Result, Server, builtin};
use sqlx::{PgPool, Row, types::Json};
use uuid::Uuid;

pub(super) async fn ensure_approved(
    pool: &PgPool,
    owner: Uuid,
    target: &Target,
    from: &str,
) -> Result<()> {
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
    .bind(from)
    .bind(Json(capabilities))
    .bind(crate::SOURCE_GRANT_EXPIRES_AT_MS)
    .fetch_one(pool)
    .await?;
    if !approved {
        return Err(Error::Conflict(
            "The referenced Host9 binding and source grant are unavailable".into(),
        ));
    }
    Ok(())
}

pub(super) async fn load(pool: &PgPool, id: i64) -> Result<Retained> {
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
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| {
        Error::Conflict("The original routing draft, facts or fixtures are unavailable".into())
    })?;
    Ok(Retained {
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
    })
}

impl Recompile {
    pub(super) async fn record<S: Server>(&self, s: &S, checked: Checked) -> Result<i64> {
        let Checked {
            retained,
            candidate,
            cases,
        } = checked;
        let source = builtin::ROUTING;
        let mut tx = s.as_ref().begin().await?;
        sqlx::query(
            "SELECT id FROM playbook_sources WHERE id=$1 AND deleted_at_ms IS NULL FOR UPDATE",
        )
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
    .bind(&self.to_artifact)
    .bind(Json(candidate.artifact))
    .bind(&self.from_artifact)
    .bind(Json(candidate.cases))
    .bind(&retained.markdown)
    .bind(Json(cases))
    .bind(s.now_ms())
    .fetch_one(&mut *tx)
    .await?;
        tx.commit().await?;
        Ok(id)
    }
}

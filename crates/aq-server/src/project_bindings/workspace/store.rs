use super::*;
type Tx<'a> = Transaction<'a, Postgres>;

pub(super) async fn owner(tx: &mut Tx<'_>, actor: Uuid) -> Result<()> {
    let role: Option<String> = sqlx::query_scalar("SELECT role FROM users WHERE id=$1 FOR SHARE")
        .bind(actor)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?;
    if role.as_deref() != Some("owner") {
        return Err(AppError::Forbidden(
            "workspace decision requires a current owner".into(),
        ));
    }
    Ok(())
}
pub(super) async fn lock_task(tx: &mut Tx<'_>, home: Uuid, task: Uuid) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!("workspace:{home}:{task}"))
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    Ok(())
}
pub(super) async fn fence(tx: &mut Tx<'_>, id: Uuid, revision: u64) -> Result<()> {
    let present:Option<i64>=sqlx::query_scalar("SELECT revision FROM project_execution_bindings WHERE project_id=$1 AND revision=$2 AND binding @> '{\"enabled\":true}'::jsonb FOR SHARE")
        .bind(id).bind(revision as i64).fetch_optional(&mut **tx).await.map_err(db)?;
    present
        .map(|_| ())
        .ok_or_else(|| conflict("workspace project binding changed"))
}
fn permission(access: Access) -> &'static str {
    match access {
        Access::Read => "read",
        Access::Write => "write",
    }
}
pub(super) async fn request(
    tx: &mut Tx<'_>,
    home: Uuid,
    task: Uuid,
    p: &SourcePin,
    decision: Disposition,
    attempt: Option<Uuid>,
) -> Result<()> {
    let state = match decision {
        Disposition::Preauthorized => "approved",
        Disposition::NeedsApproval => "pending",
    };
    let old=sqlx::query("SELECT project_revision, repository_id, binding_generation, commit_oid, access, state FROM task_workspace_sources WHERE home_project_id=$1 AND task_id=$2 AND target_project_id=$3 FOR UPDATE")
        .bind(home).bind(task).bind(p.project_id).fetch_optional(&mut **tx).await.map_err(db)?;
    if let Some(row) = old {
        let same_revision = row.get::<i64, _>("project_revision") == p.project_revision as i64;
        let exact = same_revision
            && row.get::<Uuid, _>("repository_id") == p.repository_id
            && row.get::<i64, _>("binding_generation") == p.binding_generation as i64
            && row.get::<String, _>("commit_oid") == p.commit;
        if same_revision && !exact {
            return Err(conflict("workspace target pin changed during request"));
        }
        if exact && row.get::<String, _>("access") == permission(p.access) {
            return Ok(());
        }
        if exact && (p.access == Access::Read || row.get::<String, _>("state") == "denied") {
            return Err(conflict(
                "workspace access differs from the existing request",
            ));
        }
    } else {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM task_workspace_sources WHERE home_project_id=$1 AND task_id=$2",
        )
        .bind(home)
        .bind(task)
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
        if count >= 16 {
            return Err(conflict("a task may request up to 16 sibling projects"));
        }
    }
    sqlx::query("INSERT INTO task_workspace_sources(home_project_id,task_id,target_project_id,project_revision,repository_id,binding_generation,commit_oid,access,state,requested_attempt_id)
        VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT(home_project_id,task_id,target_project_id) DO UPDATE SET
        project_revision=EXCLUDED.project_revision,repository_id=EXCLUDED.repository_id,binding_generation=EXCLUDED.binding_generation,commit_oid=EXCLUDED.commit_oid,
        access=EXCLUDED.access,state=EXCLUDED.state,requested_attempt_id=EXCLUDED.requested_attempt_id,approved_by=NULL")
        .bind(home).bind(task).bind(p.project_id).bind(p.project_revision as i64).bind(p.repository_id)
        .bind(p.binding_generation as i64).bind(&p.commit).bind(permission(p.access)).bind(state).bind(attempt)
        .execute(&mut **tx).await.map_err(db)?;
    Ok(())
}
pub(super) async fn decision(
    tx: &mut Tx<'_>,
    actor: Uuid,
    (target, home, task): (Uuid, Uuid, Uuid),
    action: &Decision,
) -> Result<()> {
    let state = match action.decision {
        Resolution::Approved => "approved",
        Resolution::Denied => "denied",
    };
    let result=sqlx::query("UPDATE task_workspace_sources SET state=$7,approved_by=$8 WHERE home_project_id=$1 AND task_id=$2 AND target_project_id=$3
        AND project_revision=$4 AND commit_oid=$5 AND access=$6 AND (state='pending' OR (state=$7 AND approved_by=$8))")
        .bind(home).bind(task).bind(target).bind(action.project_revision).bind(&action.commit_oid)
        .bind(permission(action.access)).bind(state).bind(actor).execute(&mut **tx).await.map_err(db)?;
    if result.rows_affected() != 1 {
        return Err(conflict("workspace request changed or was already decided"));
    }
    Ok(())
}
async fn sources(
    s: &AppState,
    home: Option<Uuid>,
    task: Option<Uuid>,
    target: Option<Uuid>,
    pending: bool,
) -> Result<Vec<Value>> {
    sqlx::query_scalar("SELECT to_jsonb(w) FROM task_workspace_sources w WHERE ($1::uuid IS NULL OR home_project_id=$1)
        AND ($2::uuid IS NULL OR task_id=$2) AND ($3::uuid IS NULL OR target_project_id=$3) AND (NOT $4 OR state='pending')
        ORDER BY home_project_id,task_id,target_project_id LIMIT 257")
        .bind(home).bind(task).bind(target).bind(pending).fetch_all(&s.store.pool).await.map_err(db)
}
pub(super) async fn inbox(s: &AppState, target: Uuid) -> Result<Value> {
    let mut rows = sources(s, None, None, Some(target), true).await?;
    let has_more = rows.len() > 256;
    rows.truncate(256);
    Ok(json!({"sources":rows,"has_more":has_more}))
}
pub(super) async fn target_view(
    s: &AppState,
    home: Uuid,
    task: Uuid,
    target: Uuid,
) -> Result<Value> {
    Ok(json!({"sources":sources(s,Some(home),Some(task),Some(target),false).await?,"services":[]}))
}
pub(crate) async fn view(s: &AppState, home: Uuid, task: Uuid) -> Result<Value> {
    let sources = sources(s, Some(home), Some(task), None, false).await?;
    let services:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('service',service) FROM task_workspace_services WHERE home_project_id=$1 AND task_id=$2 ORDER BY service")
        .bind(home).bind(task).fetch_all(&s.store.pool).await.map_err(db)?;
    Ok(json!({"sources":sources,"services":services}))
}

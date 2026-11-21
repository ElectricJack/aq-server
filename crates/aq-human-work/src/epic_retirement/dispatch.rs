//! Fresh Graph reads for each epic retirement owner action, then its plan.
//!
//! The server supplies the reads (its Graph runtime and upstream schemas);
//! everything an action reads is named by its registration, not its request.
use super::settle::{Close, Decide, Evidence, Refusal, Registration, Restore};
use super::{Register, owner_created, owner_registered};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::future::Future;
use uuid::Uuid;

/// One owner action on an epic retirement registration.
pub enum Action<'a> {
    Register(&'a Register),
    Restore(&'a Restore),
    Decide(&'a Decide),
    Evidence(&'a Evidence),
    Close(&'a Close),
}
impl Action<'_> {
    /// The tasks the request itself names, which admission checks.
    pub fn tasks(&self) -> Vec<Uuid> {
        match self {
            Action::Register(r) => vec![r.adopt.approval, r.adopt.cleanup],
            _ => vec![],
        }
    }
}

/// Workspace-scoped Graph reads, each checked against its upstream schema,
/// and the server's errors for this module's refusals.
pub trait Reads: Sync {
    type Error: Send;
    /// GET `path` (with any query) under `/v1/workspaces/{workspace}/`.
    fn get(
        &self,
        path: String,
        schema: &'static str,
    ) -> impl Future<Output = Result<Value, Self::Error>> + Send;
    /// An invalid request for a refusal (its reason is for the log), or an
    /// unavailable Graph for an inconsistent read.
    fn fail(&self, refusal: Option<Refusal>) -> Self::Error;
}

/// Who acts where: the authenticated owner, the bound workspace and project,
/// and the configured Graph service that dispatches.
#[derive(Clone, Copy)]
struct Scope<'a> {
    workspace: Uuid,
    actor: Uuid,
    project: Uuid,
    service: &'a str,
}

type Read<T, R> = Result<T, <R as Reads>::Error>;

async fn get<R: Reads>(r: &R, at: Scope<'_>, path: String, schema: &'static str) -> Read<Value, R> {
    r.get(format!("/v1/workspaces/{}/{path}", at.workspace), schema)
        .await
}

/// A node of this workspace with this ID and kind, including a tombstone.
async fn any<R: Reads>(r: &R, at: Scope<'_>, id: Uuid, kind: &str) -> Read<Value, R> {
    let raw = get(r, at, format!("nodes/{id}"), "Node").await?;
    if raw["id"] != json!(id) || raw["workspace_id"] != json!(at.workspace) || raw["kind"] != kind {
        return Err(r.fail(None));
    }
    Ok(raw)
}
/// A live node; a deleted one is refused until it is restored.
async fn live<R: Reads>(r: &R, at: Scope<'_>, id: Uuid, kind: &str) -> Read<Value, R> {
    let raw = any(r, at, id, kind).await?;
    if raw["deleted"] != false {
        return Err(r.fail(Some("a registered record is deleted")));
    }
    crate::projection::node(&raw, kind).ok_or_else(|| r.fail(None))?;
    Ok(raw)
}
async fn context<R: Reads>(r: &R, at: Scope<'_>, id: Uuid) -> Read<Value, R> {
    live(r, at, id, "task").await?;
    let raw = get(r, at, format!("nodes/{id}/context"), "Context").await?;
    if raw["node"]["id"] != json!(id) || raw["node"]["deleted"] != false {
        return Err(r.fail(None));
    }
    Ok(raw)
}
async fn gates<R: Reads>(r: &R, at: Scope<'_>, reg: &Registration) -> Read<[Value; 3], R> {
    Ok([
        context(r, at, reg.approval).await?,
        context(r, at, reg.cleanup).await?,
        context(r, at, reg.closure).await?,
    ])
}
async fn records<R: Reads>(
    r: &R,
    at: Scope<'_>,
    reg: &Registration,
) -> Read<BTreeMap<Uuid, Value>, R> {
    let mut nodes = BTreeMap::new();
    for id in reg.anchors() {
        nodes.insert(id, live(r, at, id, "knowledge").await?);
    }
    Ok(nodes)
}

/// The Graph batch for `action` against the subject's fresh read `subject`.
pub async fn plan<R: Reads>(
    r: &R,
    pool: &sqlx::PgPool,
    (workspace, actor, project, service): (Uuid, Uuid, Uuid, &str),
    subject: &Value,
    action: Action<'_>,
) -> Read<Vec<Value>, R> {
    let at = Scope {
        workspace,
        actor,
        project,
        service,
    };
    if let Action::Register(payload) = action {
        let approval = context(r, at, payload.adopt.approval).await?;
        let cleanup = context(r, at, payload.adopt.cleanup).await?;
        let owned = owner_created(pool, at.actor, at.workspace, payload).await;
        if !owned.map_err(|_| r.fail(None))? {
            return Err(r.fail(Some("the adopted tasks are not this owner's")));
        }
        return payload
            .plan(
                at.workspace,
                at.actor,
                at.service,
                subject,
                &approval,
                &cleanup,
            )
            .ok_or_else(|| r.fail(Some("the adopted tasks cannot be registered")));
    }
    let reg = Registration::load(subject, at.workspace, at.actor, at.service, at.project)
        .map_err(|why| r.fail(Some(why)))?;
    let planned = match action {
        Action::Register(_) => unreachable!("planned above"),
        Action::Restore(payload) => {
            let epic = live(r, at, reg.epic, "task").await?;
            let tombstone = any(r, at, reg.closure, "task").await?;
            let path = format!("nodes/{}/revisions/{}", reg.closure, payload.revision);
            let snapshot = get(r, at, path, "RevisionSnapshot").await?;
            if snapshot["node"]["workspace_id"] != json!(at.workspace) {
                return Err(r.fail(None));
            }
            payload.plan(&reg, &epic, &tombstone, &snapshot)
        }
        Action::Decide(payload) => {
            let [approval, cleanup, closure] = gates(r, at, &reg).await?;
            // A draft decision needs no journal read; a reconciliation does.
            let journaled = match reg.operation() {
                Some(op) if Decide::released(&approval) => {
                    let scope = (at.actor, at.workspace, at.project);
                    owner_registered(pool, scope, op, reg.id)
                        .await
                        .map_err(|_| r.fail(None))?
                }
                _ => false,
            };
            payload.plan(&reg, &approval, &cleanup, &closure, journaled)
        }
        Action::Evidence(payload) => {
            let [approval, cleanup, closure] = gates(r, at, &reg).await?;
            let records = records(r, at, &reg).await?;
            payload.plan(&reg, [&approval, &cleanup, &closure], &records)
        }
        Action::Close(payload) => {
            let epic = live(r, at, reg.epic, "task").await?;
            let [approval, cleanup, closure] = gates(r, at, &reg).await?;
            let records = records(r, at, &reg).await?;
            let tree = get(
                r,
                at,
                format!("nodes/{}/tree?depth=1&limit=200", reg.epic),
                "Tree",
            )
            .await?;
            payload.plan(
                &reg,
                &epic,
                [&approval, &cleanup, &closure],
                &records,
                &tree,
            )
        }
    };
    planned.map_err(|why| r.fail(Some(why)))
}

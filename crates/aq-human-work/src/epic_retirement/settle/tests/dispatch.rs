use super::*;
use crate::epic_retirement::dispatch::{Action, plan};
/// The model served as Graph reads, the way the server's runtime serves them.
struct Served<'a>(&'a Graph, Uuid);
impl crate::epic_retirement::dispatch::Reads for Served<'_> {
    type Error = String;
    async fn get(&self, path: String, _: &'static str) -> Result<Value, String> {
        let prefix = format!("/v1/workspaces/{}/nodes/", self.1);
        let rest = path.strip_prefix(&prefix).ok_or("another workspace")?;
        let (id, tail) = rest.split_once('/').unwrap_or((rest, ""));
        let id: Uuid = id.parse().map_err(|_| "malformed path")?;
        if !self.0.nodes.contains_key(&id) {
            return Err("not found".into());
        }
        let mut raw = match tail {
            "" => self.0.get(id),
            "context" => self.0.context(id),
            "tree?depth=1&limit=200" => self.0.tree(id),
            tail => {
                let n = tail.strip_prefix("revisions/").and_then(|n| n.parse().ok());
                let n: i64 = n.ok_or("malformed path")?;
                self.0.revisions.get(&(id, n)).cloned().ok_or("not found")?
            }
        };
        let node = if raw.get("node").is_some() {
            &mut raw["node"]
        } else {
            &mut raw
        };
        node["workspace_id"] = json!(self.1);
        Ok(raw)
    }
    fn fail(&self, refusal: Option<Refusal>) -> String {
        refusal.unwrap_or("unavailable").into()
    }
}

impl Served<'_> {
    fn reads<'a>(
        &'a self,
        pool: &'a sqlx::PgPool,
    ) -> crate::epic_retirement::dispatch::GraphReads<'a, Self> {
        crate::epic_retirement::dispatch::GraphReads { graph: self, pool }
    }
}
#[tokio::test]
async fn dispatch_reads_only_what_the_registration_names() {
    use crate::epic_retirement::Scope;
    let mut f = fixture();
    // Only registration reads a journal table; these actions never touch it.
    let pool = unreachable_database();
    let at = f.scope();
    let decide = Decide {
        operation_id: Uuid::new_v4(),
        project_id: f.project,
        registration_id: f.registration,
        expected_version: f.version(),
        revision: 1,
        choice: Choice::Approve,
        vetted: REPOS.iter().map(|r| (r.to_string(), sha(1))).collect(),
        evidence: artifact(0xd0),
    };
    let restore = f.restore(f.guarded);
    let subject = f.graph.get(f.registration);
    let operations = {
        let r = Served(&f.graph, f.workspace);
        let refused = plan(r.reads(&pool), at, &subject, Action::Decide(&decide)).await;
        assert_eq!(refused.unwrap_err(), "a registered record is deleted");
        let other = Scope {
            actor: Uuid::new_v4(),
            ..at
        };
        let refused = plan(r.reads(&pool), other, &subject, Action::Restore(&restore)).await;
        assert_eq!(
            refused.unwrap_err(),
            "registration belongs to another owner"
        );
        let elsewhere = Served(&f.graph, Uuid::new_v4());
        let refused = plan(
            elsewhere.reads(&pool),
            at,
            &subject,
            Action::Restore(&restore),
        )
        .await;
        assert!(refused.is_err());
        plan(r.reads(&pool), at, &subject, Action::Restore(&restore))
            .await
            .unwrap()
    };
    assert_eq!(operations, f.plan_restore(f.guarded).unwrap());
    f.graph.apply(&operations).unwrap();
    let subject = f.graph.get(f.registration);
    let r = Served(&f.graph, f.workspace);
    let decided = plan(r.reads(&pool), at, &subject, Action::Decide(&decide))
        .await
        .unwrap();
    f.graph.apply(&decided).unwrap();
    let close = Close {
        operation_id: Uuid::new_v4(),
        project_id: f.project,
        registration_id: f.registration,
        expected_version: f.version(),
    };
    let subject = f.graph.get(f.registration);
    let r = Served(&f.graph, f.workspace);
    let pending = plan(r.reads(&pool), at, &subject, Action::Close(&close)).await;
    assert_eq!(pending.unwrap_err(), "a cleanup merge is pending");
}

#[tokio::test]
async fn released_cleanup_requires_a_journal_read() {
    let pool = unreachable_database();
    // A release before any decision is reconciled only after the journal
    // read, which this unreachable pool cannot answer.
    let f = released_before_decision();
    let at = f.scope();
    let subject = f.graph.get(f.registration);
    let decide = f.decision(1, Choice::Approve);
    let r = Served(&f.graph, f.workspace);
    let unread = plan(r.reads(&pool), at, &subject, Action::Decide(&decide)).await;
    assert_eq!(unread.unwrap_err(), "unavailable");
}

fn unreachable_database() -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(200))
        .connect_lazy("postgres://localhost/unused")
        .unwrap()
}

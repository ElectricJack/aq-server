use super::*;
use aq_notifications::policy::FloodWindow;
use sqlx::postgres::PgRow;

struct Pending {
    window: FloodWindow,
    request: Value,
}
impl Pending {
    async fn freeze(
        row: &PgRow,
        recipient: Uuid,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<Self> {
        let project: Uuid = row.get("project_id");
        let workspace: Uuid = row.get("workspace_id");
        let start: i64 = row.get("window_start_ms");
        let window = FloodWindow {
            scope: Scope {
                project_id: project,
                workspace_id: workspace,
            },
            start,
        };
        let request = match row.get::<Option<Value>, _>("summary") {
            Some(v) => v,
            None => {
                let collapsed: i32 = row.get("collapsed");
                let kinds: Vec<String> = row.get("kinds");
                let send = window.summary(recipient, collapsed, &kinds);
                let v = serde_json::to_value(&send)?;
                sqlx::query("UPDATE notification_flood_windows SET summary=$4 WHERE project_id=$1 AND workspace_id=$2 AND window_start_ms=$3")
                        .bind(project).bind(workspace).bind(start).bind(&v).execute(&mut **tx).await?;
                v
            }
        };
        Ok(Self { window, request })
    }
    async fn submit(self, policy: &Policy, recipient: Uuid) -> Result<bool> {
        let request: SendRequest = serde_json::from_value(self.request)?;
        // A failed submission leaves the window pending for the next tick.
        if policy.service.enqueue(recipient, request).await.is_err() {
            return Ok(false);
        }
        let scope = self.window.scope;
        sqlx::query("UPDATE notification_flood_windows SET submitted=TRUE WHERE project_id=$1 AND workspace_id=$2 AND window_start_ms=$3")
                    .bind(scope.project_id).bind(scope.workspace_id).bind(self.window.start).execute(&policy.service.pool).await?;
        Ok(true)
    }
}

impl Policy {
    /// Submits one counted summary per closed flood window that collapsed
    /// episodes. The summary request is frozen before submission so a retry
    /// after a crash re-submits the identical request. Returns windows flushed.
    pub async fn flush(&self) -> Result<usize> {
        let now = self.service.now()? as i64;
        let mut tx = self.service.pool.begin().await?;
        lock(&mut tx).await?;
        let rows = sqlx::query("SELECT project_id,workspace_id,window_start_ms,collapsed,kinds,summary FROM notification_flood_windows WHERE collapsed>0 AND NOT submitted AND window_start_ms+$1<=$2 ORDER BY window_start_ms LIMIT 8 FOR UPDATE")
            .bind(FLOOD_WINDOW_MS as i64).bind(now).fetch_all(&mut *tx).await?;
        if rows.is_empty() {
            return Ok(0);
        }
        let recipient = owner(&mut tx).await?;
        let mut pending = Vec::new();
        for row in rows {
            pending.push(Pending::freeze(&row, recipient, &mut tx).await?);
        }
        tx.commit().await?;
        let mut flushed = 0;
        for publication in pending {
            flushed += usize::from(publication.submit(self, recipient).await?);
        }
        Ok(flushed)
    }
}

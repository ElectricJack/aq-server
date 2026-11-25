use super::*;

// A node the owner removed in worker settings is not a worker that needs attention.
const WORKER: &str = "SELECT id,name,worker_version,protocol_version,enrolled_at,last_seen_at,revoked_at,xmin::text AS revision FROM nodes WHERE id=$1 AND revoked_at IS NULL AND NOT EXISTS(SELECT 1 FROM worker_settings s WHERE s.kind='node' AND s.removed AND s.id=nodes.id::text)";
const WORKERS: &str = "SELECT id,name,worker_version,protocol_version,enrolled_at,last_seen_at,revoked_at,xmin::text AS revision FROM nodes WHERE revoked_at IS NULL AND NOT EXISTS(SELECT 1 FROM worker_settings s WHERE s.kind='node' AND s.removed AND s.id=nodes.id::text) ORDER BY id LIMIT 101";
fn offline(
    state: &AppState,
    w: &sqlx::postgres::PgRow,
    project: Option<ProjectRef>,
) -> Result<Option<AttentionItem>> {
    let seen: Option<chrono::DateTime<chrono::Utc>> = w.get("last_seen_at");
    let worker = crate::store::Node {
        id: w.get("id"),
        name: w.get("name"),
        worker_version: w.get("worker_version"),
        protocol_version: w.get::<i32, _>("protocol_version") as u32,
        enrolled_at: w.get("enrolled_at"),
        last_seen_at: seen,
        revoked_at: w.get("revoked_at"),
    };
    let after = Duration::from_secs(state.config.workers.offline_after_secs);
    if crate::admin_api::worker_status(&worker, (state.clock)(), after) != "offline" {
        return Ok(None);
    }
    let since = seen
        .map(|at| {
            timestamp((at.timestamp_millis() as u64).saturating_add(after.as_millis() as u64))
        })
        .transpose()?;
    Ok(Some(attention(
        AttentionKind::WorkerOffline,
        reference("worker", worker.id, Revision(w.get("revision")).into()),
        project,
        AttentionDescription {
            title: "Worker offline",
            reason: if seen.is_some() {
                "Heartbeat exceeds the configured offline threshold"
            } else {
                "No heartbeat has been observed"
            },
            since,
        },
    )))
}
#[derive(Default)]
struct Activity {
    running: usize,
    recovering: usize,
    unknown_running: bool,
    per_project_running: BTreeMap<Uuid, u32>,
    workers: BTreeMap<(Uuid, Option<Uuid>), AttentionItem>,
}
impl OverviewRead<'_> {
    pub(super) async fn attempts(&self, installation: bool) -> Result<Contribution> {
        let state = &self.request.state;
        let runtime = &self.request.runtime;
        let ids = &self.ids;
        let truncated = self.truncated;
        let mut transaction = tx(runtime).await?;
        let rows = sqlx::query("SELECT attempt_id,node_id FROM execution_attempts WHERE (CASE WHEN kind='profiled_v1' THEN state#>>'{offer,manifest,bindings,project_id}' ELSE state#>>'{offer,spec,project_id}' END)=ANY($1) ORDER BY attempt_id LIMIT 101")
        .bind(ids.iter().map(ToString::to_string).collect::<Vec<_>>()).fetch_all(&mut *transaction).await?;
        transaction.commit().await?;
        let more = truncated || rows.len() > MEMBERS;
        let mut c = Contribution::default();
        let mut activity = Activity::default();
        for row in rows.iter().take(MEMBERS) {
            activity.observe(self, row).await?;
        }
        c.sources.push(source(
            "attempts",
            None,
            SourceSample {
                at: stamp(state)?,
                count: rows.len().min(MEMBERS),
                truncated: more,
            },
            None,
        )?);
        // SO-06: across all projects every enrolled offline worker is attention,
        // not only those linked to retained attempts (those keep their project).
        let nodes_more = if installation {
            activity.worker_inventory(self.request).await?
        } else {
            false
        };
        c.sources.push(source(
            "workers",
            None,
            SourceSample {
                at: stamp(state)?,
                count: activity.workers.len(),
                truncated: more || nodes_more,
            },
            (!installation).then_some("association_from_retained_attempts"),
        )?);
        c.attention.extend(activity.workers.into_values());
        c.running = (!activity.unknown_running).then(|| total(activity.running, more));
        if activity.unknown_running {
            c.sources.push(unavailable(
                "running",
                "process_start_observation_unavailable",
            ));
        }
        c.recovering = Some(total(activity.recovering, more));
        c.per_project_running = activity.per_project_running;
        Ok(c)
    }
}

impl Activity {
    fn count(&mut self, state: &AppState, v: &authority::AuthorityView) -> Result<()> {
        if v.running_at(stamp(state)?) {
            self.running += 1;
            *self.per_project_running.entry(v.project_id).or_insert(0) += 1;
        } else if v.active_at(stamp(state)?).is_ok() {
            self.unknown_running = true;
        }
        let awaiting_retention =
            v.stopped && matches!(v.retention, Some(Retention::Pending | Retention::Unknown));
        let interrupted = matches!(v.status, Status::Cancelling | Status::Unknown);
        let pending_recovery = interrupted || awaiting_retention;
        if v.holds_task && pending_recovery {
            self.recovering += 1;
        }
        Ok(())
    }
    async fn observe(
        &mut self,
        read: &OverviewRead<'_>,
        row: &sqlx::postgres::PgRow,
    ) -> Result<()> {
        let state = &read.request.state;
        let mut transaction = tx(&read.request.runtime).await?;
        let node = row.get("node_id");
        let attempt = row.get("attempt_id");
        let v =
            authority::read_attempt(state, &mut transaction, node, attempt, Policy::IdentityOnly)
                .await
                .map_err(|_| Error::Unavailable)?;
        if !read.ids.contains(&v.project_id) {
            return Err(Error::Unavailable);
        }
        self.count(state, &v)?;
        let worker = sqlx::query(WORKER)
            .bind(node)
            .fetch_optional(&mut *transaction)
            .await?;
        if let Some(w) = worker
            && let Some(item) = offline(state, &w, read.labels.get(&v.project_id).cloned())?
        {
            self.workers.insert((node, Some(v.project_id)), item);
        }
        transaction.commit().await?;
        Ok(())
    }
    async fn worker_inventory(&mut self, request: &DashboardRequest) -> Result<bool> {
        let (state, runtime, _) = request.parts();
        let mut transaction = tx(runtime).await?;
        let rows = sqlx::query(WORKERS).fetch_all(&mut *transaction).await?;
        transaction.commit().await?;
        let nodes_more = rows.len() > MEMBERS;
        for w in rows.iter().take(MEMBERS) {
            let id: Uuid = w.get("id");
            if !self.workers.keys().any(|(node, _)| *node == id)
                && let Some(item) = offline(state, w, None)?
            {
                self.workers.insert((id, None), item);
            }
        }
        Ok(nodes_more)
    }
}

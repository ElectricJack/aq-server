use super::*;
use crate::execution::authority::AuthorityView;

pub(super) struct AuthorizedAttempt {
    pub attempt: Uuid,
    pub view: AuthorityView,
    pub access: session_access::InspectionAccess,
}
impl AuthorizedAttempt {
    pub(super) async fn load(
        request: &DashboardRequest,
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
        scope: &Scope,
    ) -> Result<Self> {
        let (state, _, user) = request.parts();
        let row =
            sqlx::query("SELECT attempt_id,node_id FROM execution_attempts WHERE session_id=$1")
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?
                .ok_or_else(|| AppError::NotFound("session not found".into()))?;
        let attempt: Uuid = row.get("attempt_id");
        let node: Uuid = row.get("node_id");
        let view = authority::read_attempt(state, tx, node, attempt, Policy::TerminalHistorical)
            .await
            .map_err(AppError::from)?;
        if scope.project().is_some_and(|p| p != view.project_id) {
            return Err(AppError::NotFound("session not found".into()).into());
        }
        let access = session_access::require_inspect_locked(state, tx, &view, user.id).await?;
        Ok(Self {
            attempt,
            view,
            access,
        })
    }
}
struct Inspected {
    id: Uuid,
    attempt: Uuid,
    view: AuthorityView,
    access: session_access::InspectionAccess,
    session_identity: SessionIdentity,
    project: ProjectRef,
}
impl Inspected {
    async fn load(
        request: &DashboardRequest,
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
        scope: &Scope,
    ) -> Result<Self> {
        let AuthorizedAttempt {
            attempt,
            view,
            access,
        } = AuthorizedAttempt::load(request, tx, id, scope).await?;
        let (actual, session_identity) = identity(&view.session);
        if actual != id {
            return Err(Error::Unavailable);
        }
        let project = project_label(tx, view.project_id, access.owner).await?;
        Ok(Self {
            id,
            attempt,
            view,
            access,
            session_identity,
            project,
        })
    }
    async fn permissions(&self, tx: &mut Transaction<'_, Postgres>) -> Result<Vec<String>> {
        let mut permissions = vec!["session.inspect".to_string()];
        if self.access.owner {
            // The Message slot needs a live scoped mailbox: an unrevoked launch binding.
            let mailbox: Option<bool> = sqlx::query_scalar(
                "SELECT NOT revoked FROM agent_launch_bindings WHERE attempt_id=$1",
            )
            .bind(self.attempt)
            .fetch_optional(&mut **tx)
            .await?;
            if mailbox == Some(true) && !self.view.revoked {
                permissions.push(crate::agent_access::messaging::session_action::KIND.into());
                permissions.push(crate::agent_access::messaging::session_action::NUDGE_KIND.into());
            }
        }
        Ok(permissions)
    }
    async fn terminal(
        &self,
        state: &AppState,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<TerminalOutput> {
        let mut terminal = None;
        let mut output_ms = None;
        if !self.view.revoked
            && let Some(config) = state.config.terminals.as_ref()
        {
            let registered=sqlx::query("SELECT terminal_id,identity,terminal_epoch,last_output_at_ms,runtime_id FROM terminal_sessions WHERE attempt_id=$1").bind(self.attempt).fetch_optional(&mut **tx).await?;
            if let Some(row) = registered {
                let exact: aq_session_protocol::TerminalIdentity =
                    serde_json::from_value(row.get("identity")).map_err(|_| Error::Unavailable)?;
                if exact.session == self.view.session
                    && row.get::<Uuid, _>("runtime_id") == config.runtime_id
                {
                    terminal = Some(EntityRef {
                        kind: "terminal".into(),
                        id: row.get("terminal_id"),
                        revision: Some(
                            Revision(row.get::<i64, _>("terminal_epoch").to_string()).into(),
                        ),
                    });
                    let at: i64 = row.get("last_output_at_ms");
                    if at > 0 {
                        output_ms = Some(at as u64);
                    }
                }
            }
        }
        Ok(TerminalOutput {
            terminal,
            output_ms,
        })
    }
    fn summary(
        &self,
        profile: &LaunchProfile,
        output: &TerminalOutput,
        observation: ProcessObservation,
    ) -> Result<SessionSummary> {
        let status = serde_json::to_value(self.view.status)
            .map_err(|_| Error::Unavailable)?
            .as_str()
            .ok_or(Error::Unavailable)?
            .to_owned();
        Ok(SessionSummary {
            id: self.id,
            label: format!(
                "{} session {}",
                if self.view.kind == Kind::LegacyFixture {
                    "Fixture"
                } else {
                    "Remote"
                },
                self.id
            ),
            task: Some(TitledRef {
                kind: "task".into(),
                id: self.view.task_id,
                revision: profile.task_revision.clone().map(Into::into),
                title: aq_swarm::agents::title_or_id(self.view.task_id),
            }),
            attempt_id: Some(self.attempt),
            project: self.project.clone(),
            profile: profile.profile.clone(),
            authority: "remote".into(),
            // Decided now and fresh for one envelope interval, never instantly stale (RA-16).
            observation,
            wait: None,
            controller: Controller {
                state: "unknown".into(),
                label: None,
            },
            terminal: output.terminal.clone(),
            session_identity: self.session_identity.clone(),
            lifecycle: profile.lifecycle.clone(),
            record_revision: if self.view.record_revision > 0 {
                Some(Revision(self.view.record_revision.to_string()))
            } else {
                None
            },
            execution_authority: status,
            source_kind: if self.view.kind == Kind::LegacyFixture {
                "legacy_fixture"
            } else {
                "profiled_v1"
            }
            .into(),
            last_output_at: output.output_ms.map(timestamp).transpose()?,
        })
    }
}

struct TerminalOutput {
    terminal: Option<EntityRef>,
    output_ms: Option<u64>,
}
#[path = "session/profile.rs"]
mod profile;
use profile::LaunchProfile;
pub(super) use profile::matches_offer;
fn observation(facts: &crate::stall::freshness::Facts, now: u64) -> Result<ProcessObservation> {
    // The ladder's own windows decide the state (`stall::runtime::run`).
    let (observation_state, reason) =
        crate::stall::freshness::state(&facts, &crate::stall::policy::ExecutionPolicy::v1(), now);
    Ok(ProcessObservation {
        state: observation_state.as_str().into(),
        observed_at: timestamp(now)?,
        fresh_until: timestamp(now.saturating_add(5000))?,
        reason: reason.map(Into::into),
        last_observation_at: facts.last_observation_ms.map(timestamp).transpose()?,
        last_progress_at: facts.last_progress_ms.map(timestamp).transpose()?,
    })
}
pub(in crate::dashboard_api) async fn session(
    request: &DashboardRequest,
    id: Uuid,
    scope: &Scope,
    // The list reads the observation facts of its whole page in one query.
    observed: Option<&std::collections::HashMap<Uuid, crate::stall::freshness::Facts>>,
) -> Result<SessionDetail> {
    let (state, runtime, _) = request.parts();
    if id.is_nil() {
        return Err(Error::Invalid);
    }
    // Keep all authority and execution reads for a single session in its snapshot.
    let mut tx = tx(runtime).await?;
    let record = Inspected::load(request, &mut tx, id, scope).await?;
    let profile = LaunchProfile::read(&record, &mut tx).await?;
    let output = record.terminal(state, &mut tx).await?;
    let permissions = record.permissions(&mut tx).await?;
    let now = stamp(state)?;
    if now < record.access.checked_at_ms || now >= record.access.valid_until_ms {
        return Err(Error::AuthorizationChanged);
    }
    let facts = match observed {
        Some(page) => page.get(&id).copied(),
        None => crate::observations::freshness(&state.store.pool, &[id])
            .await?
            .remove(&id),
    }
    .unwrap_or_default();
    let summary = record.summary(&profile, &output, observation(&facts, now)?)?;
    let ran_with = crate::role_settings::ran_with(state, &mut tx, record.attempt).await?;
    tx.commit().await?;
    Ok(SessionDetail {
        summary,
        profile_revisions: profile.revisions,
        permissions,
        last_activity_at: output
            .output_ms
            .max(facts.last_progress_ms)
            .map(timestamp)
            .transpose()?,
        observation_binding: profile.observation_binding,
        ran_with,
    })
}

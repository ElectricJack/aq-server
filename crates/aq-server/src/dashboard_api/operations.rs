//! Owner-only G10 operations read for one session. Every fact reuses its owner's
//! durable record: H3 stall episodes, quarantines and nudges, the H4 wait
//! snapshot, the H2 lineage ledger, H7 terminal registration and the retained
//! launch manifest. Credentials in the manifest are never projected.
use super::*;
use crate::stall::{evidence::PROGRESS_SQL, ladder::Episode};

fn ms(v: i64) -> Result<Timestamp> {
    timestamp(u64::try_from(v).map_err(|_| Error::Unavailable)?)
}
fn text<T: serde::Serialize>(v: &T) -> Result<String> {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .ok_or(Error::Unavailable)
}
fn source(a: &aq_session_protocol::ArtifactRef) -> ArtifactSource {
    ArtifactSource {
        id: a.id.as_str().into(),
        version: a.version.as_str().into(),
        digest: Digest(format!("sha256:{}", a.digest.as_str())),
    }
}
fn scopes(s: &[aq_session_protocol::Scope]) -> Vec<PermissionScope> {
    s.iter()
        .map(|s| PermissionScope {
            operation: s.operation.as_str().into(),
            resource: s.resource.as_str().into(),
        })
        .collect()
}

pub(in crate::dashboard_api) async fn operations(
    state: &AppState,
    runtime: &Runtime,
    user: &User,
    id: Uuid,
    scope: Scope,
) -> Result<Envelope<SessionOperations>> {
    let a = auth(state, runtime, user).await?;
    if !a.owner || !a.visibility_complete {
        return Err(AppError::Forbidden("session operations require an owner".into()).into());
    }
    if id.is_nil() {
        return Err(Error::Invalid);
    }
    let mut tx = tx(runtime).await?;
    let row = sqlx::query("SELECT attempt_id,node_id FROM execution_attempts WHERE session_id=$1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::NotFound("session not found".into()))?;
    let attempt: Uuid = row.get("attempt_id");
    let node: Uuid = row.get("node_id");
    let view = authority::read_attempt(state, &mut tx, node, attempt, Policy::TerminalHistorical)
        .await
        .map_err(AppError::from)?;
    if scope.project().is_some_and(|p| p != view.project_id) {
        return Err(AppError::NotFound("session not found".into()).into());
    }
    let access = session_access::require_inspect_locked(state, &mut tx, &view, user.id).await?;
    if !access.owner {
        return Err(AppError::Forbidden("session operations require an owner".into()).into());
    }
    if identity(&view.session).0 != id {
        return Err(Error::Unavailable);
    }

    // Retained launch manifest, only when it is bound to this exact attempt.
    let mut offer = None;
    if view.kind == Kind::ProfiledV1 {
        let raw: Value =
            sqlx::query_scalar("SELECT state->'offer' FROM execution_attempts WHERE attempt_id=$1")
                .bind(attempt)
                .fetch_one(&mut *tx)
                .await?;
        let o: ProfiledOffer = serde_json::from_value(raw).map_err(|_| Error::Unavailable)?;
        o.validate().map_err(|_| Error::Unavailable)?;
        if o.binding.session != view.session
            || o.binding.attempt_id != attempt
            || o.binding.offer_id != view.offer_id
            || Some(&o.binding.launch_manifest_digest) != view.launch_manifest_digest.as_ref()
            || o.binding.execution_spec_digest.as_str() != view.execution_spec_digest
        {
            return Err(Error::Unavailable);
        }
        offer = Some(o);
    }

    // H3 episode and its rung-5 quarantine.
    let stall = match sqlx::query("SELECT e.state,e.revision,e.updated_at_ms,q.lineage_id AS q_lineage,q.reason,q.escalation,q.created_at_ms AS q_at FROM stall_episodes e LEFT JOIN stall_quarantines q ON q.attempt_id=e.attempt_id WHERE e.attempt_id=$1")
        .bind(attempt)
        .fetch_optional(&mut *tx)
        .await?
    {
        None => None,
        Some(r) => {
            let ep: Episode =
                serde_json::from_value(r.get("state")).map_err(|_| Error::Unavailable)?;
            let quarantine = match r.get::<Option<Uuid>, _>("q_lineage") {
                Some(lineage_id) => Some(StallQuarantine {
                    scope: "session_and_lineage".into(),
                    lineage_id,
                    reason: r.get("reason"),
                    escalation: r.get("escalation"),
                    at: ms(r.get("q_at"))?,
                }),
                None => None,
            };
            Some(StallState {
                rung: text(&ep.rung)?,
                revision: u64::try_from(r.get::<i64, _>("revision"))
                    .map_err(|_| Error::Unavailable)?,
                rung_since: timestamp(ep.rung_since_ms)?,
                updated_at: ms(r.get("updated_at_ms"))?,
                explanation: ep.explanation.as_ref().map(text).transpose()?,
                observed_through: ep.observed_through_ms.map(timestamp).transpose()?,
                nudges: ep.nudges,
                deferrals: ep.deferrals,
                notified: ep.notified,
                incidents: ep.incidents,
                quarantine,
            })
        }
    };

    // The same semantic-progress predicate the stall ladder judges by.
    let progress: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT max(recorded_at_ms) FROM observation_events WHERE attempt_id=$1 AND {PROGRESS_SQL}"
    ))
    .bind(attempt)
    .fetch_one(&mut *tx)
    .await?;
    let semantic_progress = progress
        .map(|at| {
            Ok::<_, Error>(ProgressObservation {
                observed_at: ms(at)?,
            })
        })
        .transpose()?;

    // H4: the attempt's live wait, else its newest one.
    let wait = match sqlx::query("SELECT wait_id,phase,revision,next_due_ms,deadline_ms,snapshot FROM agent_waits WHERE attempt_id=$1 ORDER BY (phase<>'terminal') DESC,created_at_ms DESC,wait_id LIMIT 1")
        .bind(attempt)
        .fetch_optional(&mut *tx)
        .await?
    {
        None => None,
        Some(r) => {
            let snapshot = aq_wait_engine::Snapshot::decode(&r.get::<Vec<u8>, _>("snapshot"))
                .map_err(|_| Error::Unavailable)?;
            let checked = snapshot
                .member_states()
                .iter()
                .filter_map(|m| m.last_checked_at_ms)
                .max();
            let positive = |v: i64| (v > 0).then(|| ms(v)).transpose();
            Some(WaitObservation {
                id: r.get("wait_id"),
                phase: r.get("phase"),
                revision: u64::try_from(r.get::<i64, _>("revision"))
                    .map_err(|_| Error::Unavailable)?,
                observed_at: checked.map(timestamp).transpose()?,
                next_due: positive(r.get("next_due_ms"))?,
                deadline: positive(r.get("deadline_ms"))?,
                hold_reason: snapshot.hold_reason().map(|t| t.as_str().to_owned()),
            })
        }
    };

    // H3 nudges with the same F3 delivery rules the ladder folds in.
    let nudges = sqlx::query("SELECT n.message_id,n.action_id IS NULL AS owner,n.inbox_only,n.created_at_ms, \
        (SELECT min(acknowledged_at_ms) FROM mailbox_receipts r WHERE r.message_id=n.message_id) AS acked_at, \
        (SELECT d.disposition FROM mailbox_deliveries d WHERE d.message_id=n.message_id AND d.attempt_id=n.attempt_id) AS disposition, \
        (SELECT d.updated_at_ms FROM mailbox_deliveries d WHERE d.message_id=n.message_id AND d.attempt_id=n.attempt_id) AS disposed_at \
        FROM stall_nudges n WHERE n.attempt_id=$1 ORDER BY n.created_at_ms DESC,n.message_id LIMIT 50")
        .bind(attempt)
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(|r| {
            let acked: Option<i64> = r.get("acked_at");
            let disposition: Option<String> = r.get("disposition");
            let disposed: Option<i64> = r.get("disposed_at");
            let inbox_only: bool = r.get("inbox_only");
            let (delivery, at) = match (acked, disposition.as_deref()) {
                (Some(at), _) => ("delivered", Some(at)),
                (None, Some("handed_to_harness")) => ("delivered", disposed),
                (None, Some("delivery_unknown")) => ("unknown", None),
                _ if inbox_only => ("inbox_only", None),
                _ => ("queued", None),
            };
            Ok(NudgeObservation {
                message_id: r.get("message_id"),
                source: if r.get::<bool, _>("owner") { "owner" } else { "stall_ladder" }.into(),
                created_at: ms(r.get("created_at_ms"))?,
                inbox_only,
                delivery: delivery.into(),
                delivered_at: at.map(ms).transpose()?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // H7: snapshot/replay is keyed by this exact terminal identity and epoch.
    let mut terminal = None;
    if let Some(config) = state.config.terminals.as_ref()
        && let Some(r) = sqlx::query("SELECT terminal_id,identity,terminal_epoch,last_output_at_ms,runtime_id FROM terminal_sessions WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_optional(&mut *tx)
            .await?
    {
        let exact: aq_session_protocol::TerminalIdentity =
            serde_json::from_value(r.get("identity")).map_err(|_| Error::Unavailable)?;
        if exact.session == view.session && r.get::<Uuid, _>("runtime_id") == config.runtime_id {
            let at: i64 = r.get("last_output_at_ms");
            terminal = Some(TerminalReplayRef {
                terminal_id: r.get("terminal_id"),
                epoch: u64::try_from(r.get::<i64, _>("terminal_epoch"))
                    .map_err(|_| Error::Unavailable)?,
                last_output_at: (at > 0).then(|| ms(at)).transpose()?,
            });
        }
    }
    let now = stamp(state)?;
    if now < access.checked_at_ms || now >= access.valid_until_ms {
        return Err(Error::AuthorizationChanged);
    }
    tx.commit().await?;

    // H2 lineage ledger (read-only: no soft ceiling, so no writes).
    let mut lineage_usage = None;
    if let Some(graph) = view.graph.as_ref() {
        let lineage = aq_admission::transport::task_lineage_id(
            view.project_id,
            graph.workspace_id,
            view.task_id,
        )
        .map_err(|_| Error::Unavailable)?;
        if let Some(b) = crate::observations::budget::lineage(state, lineage, None)
            .await
            .map_err(|_| Error::Unavailable)?
        {
            let rate_card = offer.as_ref().and_then(|o| {
                let (provider, model) = (
                    o.manifest.model.provider.as_str(),
                    o.manifest.model.model.as_str(),
                );
                state
                    .config
                    .intelligence
                    .as_ref()?
                    .config
                    .accounting
                    .rate_cards
                    .iter()
                    .filter(|r| {
                        r.card.provider.as_str() == provider && r.card.model.as_str() == model
                    })
                    .map(|r| r.revision)
                    .max()
                    .map(|revision| RateCardRef {
                        provider: provider.into(),
                        model: model.into(),
                        revision,
                    })
            });
            lineage_usage = Some(LineageUsage {
                lineage_id: b.lineage,
                ledger_revision: b.ledger_revision,
                spent_microunits: b.spent_microunits,
                currency: b.currency,
                complete: b.accounting == aq_selection::AccountingCompleteness::Complete,
                rate_card,
            });
        }
    }

    let (instructions, requested_permissions, effective_permissions) = match offer.as_ref() {
        Some(o) => {
            let m = &o.manifest;
            (
                Some(InstructionProvenance {
                    catalog: source(&m.catalog),
                    agent_profile: source(&m.agent_profile),
                    modules: m.modules.iter().map(source).collect(),
                    context: m
                        .context
                        .references
                        .iter()
                        .map(|c| {
                            Ok(ContextSource {
                                artifact: source(&c.artifact),
                                provenance: c.provenance.as_str().into(),
                                delivery: text(&c.delivery)?,
                                required: c.required,
                            })
                        })
                        .collect::<Result<_>>()?,
                }),
                Some(scopes(&m.permissions.requested)),
                Some(scopes(&m.permissions.effective)),
            )
        }
        None => (None, None, None),
    };
    let data = SessionOperations {
        session_id: id,
        attempt_id: attempt,
        stall,
        semantic_progress,
        wait,
        nudges,
        lineage_usage,
        instructions,
        requested_permissions,
        effective_permissions,
        terminal,
    };
    fence(state, runtime, user, &a).await?;
    envelope(state, scope, data, None)
}

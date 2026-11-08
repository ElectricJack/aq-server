//! Trusted snapshot adapter. No network handler accepts these facts as launch
//! authority. AQ-08 supplies authenticated task/context and owns actual sealing.
use super::Result;
use super::*;
use crate::project_bindings::domain::FrozenProjectBinding;
use aq_selection::*;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub decision_id: Uuid,
    pub project: FrozenProjectBinding,
    pub inputs: SelectionInputs,
    pub outcome: SelectionOutcome,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveRequest {
    pub operation_id: Uuid,
    pub lineage: TaskLineage,
    pub project: FrozenProjectBinding,
    pub compositions: Vec<PreparedComposition>,
    /// Owner-assessed failed-predecessor replacement (B4). Absent for a first
    /// attempt; legacy history without it stays conservatively unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<ReplacementBudget>,
    /// False only for a scheduling poll: a refused selection is then not
    /// recorded, so refused polls cannot fill the decision table (spec 0007).
    /// Not part of the request payload or its digest.
    #[serde(skip, default = "yes")]
    pub record_refused: bool,
}
fn yes() -> bool {
    true
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplacementBudget {
    pub attempts_started: u32,
    /// Owner and service handoffs (spec 0007 Q1); not charged to the policy.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub exempt_attempts: u32,
    pub elapsed_ms: u64,
    pub evidence: ReplacementEvidence,
}
fn is_zero(v: &u32) -> bool {
    *v == 0
}
fn artifact(id: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).expect("constant"),
        version: Text::new("1").expect("constant"),
        digest: Digest::of(id.as_bytes()),
    }
}
async fn attempts(
    state: &AppState,
    lineage: &TaskLineage,
) -> Result<crate::profiled_execution::no_grant::AttemptCounts> {
    crate::profiled_execution::no_grant::attempt_counts(&state.store.pool, lineage.task_id)
        .await?
        .check_retry_bound()
}

pub(super) fn relevant(
    observations: Vec<AvailabilityObservation>,
    credentials: &[AuthorizedCredential],
    node: Uuid,
) -> Vec<AvailabilityObservation> {
    observations
        .into_iter()
        .filter(|o| {
            o.scope.node_id.is_none_or(|id| id == node)
                && credentials.iter().any(|c| {
                    c.profile == o.scope.credential_profile
                        && c.revision == o.scope.credential_revision
                        && c.provider == o.scope.provider
                        && c.account_reference == o.scope.account_reference
                        && c.endpoint_reference == o.scope.endpoint_reference
                })
        })
        .collect()
}
/// Internal authenticated-caller service, never a grant. Task/context arguments
/// must be obtained from their real owners. Replays retain their original result;
/// new observations need a new operation ID. No provider calls occur here.
pub async fn resolve_trusted(
    state: &AppState,
    actor: Uuid,
    request: ResolveRequest,
) -> Result<Decision> {
    resolve_with_principal(state, principal::Principal::human(actor)?, request).await
}
/// Scheduled selection derives its principal only from this daemon's configured
/// project policy. No user identity or worker-selected policy enters this path.
pub async fn resolve_for_project_policy(
    state: &AppState,
    request: ResolveRequest,
) -> Result<Decision> {
    let subject = principal::Principal::configured(state, request.lineage.project_id)?;
    resolve_with_principal(state, subject, request).await
}
async fn resolve_with_principal(
    state: &AppState,
    subject: principal::Principal,
    mut request: ResolveRequest,
) -> Result<Decision> {
    if request.operation_id.is_nil()
        || request.lineage.task_id.is_nil()
        || request.lineage.project_id != request.project.project.binding.project_id
        || request.lineage.workspace_id != request.project.project.binding.graph_workspace_id
    {
        return Err(bad("trusted resolver subject mismatch"));
    }
    let payload = digest(&request).map_err(|_| bad("resolver digest"))?;
    let mut early = state.store.pool.begin().await.map_err(db)?;
    subject.authorize(state, &mut early).await?;
    if let Some(decision) = subject
        .lookup(&mut early, request.operation_id, &payload)
        .await?
    {
        return Ok(decision);
    }
    early.rollback().await.map_err(db)?;
    let loaded = loaded(state)?;
    let node_id = request.project.node_id;
    let node: Value =
        sqlx::query_scalar("SELECT snapshot FROM intelligence_node_snapshots WHERE node_id=$1")
            .bind(node_id)
            .fetch_optional(&state.store.pool)
            .await
            .map_err(db)?
            .ok_or_else(|| conflict("installed harness/credential evidence unavailable"))?;
    let node: CapabilitySnapshot = serde_json::from_value(node).map_err(|_| AppError::Internal)?;
    let catalog_bytes: Vec<u8> =
        sqlx::query_scalar("SELECT artifact FROM profile_catalog_artifacts WHERE digest=$1")
            .bind(&request.project.project.binding.catalog_digest)
            .fetch_one(&state.store.pool)
            .await
            .map_err(db)?;
    let catalog = aq_profiles::ValidatedCatalog::from_artifact(&catalog_bytes)
        .map_err(|_| AppError::Internal)?;
    let credentials: Vec<_> = loaded
        .config
        .credentials
        .iter()
        .zip(&loaded.credentials)
        .filter(|(cfg, _)| cfg.allows(node_id))
        .map(|(_, c)| c.clone())
        .collect();
    let observations: Vec<Value> = sqlx::query_scalar(
        "SELECT observation FROM intelligence_observations ORDER BY scope_source_digest LIMIT 1024",
    )
    .fetch_all(&state.store.pool)
    .await
    .map_err(db)?;
    let observations = observations
        .into_iter()
        .map(serde_json::from_value)
        .collect::<std::result::Result<Vec<AvailabilityObservation>, _>>()
        .map_err(|_| AppError::Internal)?;
    let mut observations = relevant(observations, &credentials, node_id);
    let time = now(state)?;
    let registry_revision = sqlx::query_scalar::<_, i64>(
        "SELECT revision FROM intelligence_registry WHERE singleton=TRUE",
    )
    .fetch_optional(&state.store.pool)
    .await
    .map_err(db)?
    .ok_or_else(|| conflict("intelligence registry has not been installed"))?
        as u64;
    // Spec 0007 Q2: the switch's attestation (aq_machine_control::owner_attested).
    let mut conn = state.store.pool.acquire().await.map_err(db)?;
    let at = (registry_revision, time);
    let owner =
        aq_machine_control::owner_attested(&mut conn, &credentials, &observations, at.0, at.1);
    observations.extend(owner.await?);
    drop(conn);
    let count = attempts(state, &request.lineage).await?;
    let replacement = match request.replacement.take() {
        Some(r)
            if r.attempts_started == count.started
                && r.attempts_started > 0
                && r.exempt_attempts <= r.attempts_started
                && r.evidence.lineage == request.lineage =>
        {
            Some(r)
        }
        Some(_) => return Err(bad("replacement budget does not match lineage history")),
        None => None,
    };
    let placement = crate::profiled_execution::placement::facts(
        state,
        node_id,
        &request.project.project.binding.catalog_digest,
        request.lineage.project_id,
        &request.project.environment,
        time,
    )
    .await?;
    let ceiling = &request.project.project.binding.ceilings;
    // H2: lineage spend from the observation ledger. Retries and wakes share
    // the lineage, so spend accumulates. A cost limit without hard enforcement
    // is a soft ceiling and records a policy event. The ledger is never a hard
    // budget provider, so hard requirements stay refused (0004 §2.2).
    let soft = match (
        ceiling.cost_limit_microunits,
        &ceiling.currency,
        ceiling.require_hard_cost_enforcement,
    ) {
        (Some(limit_microunits), Some(currency), false) => {
            Some(crate::observations::budget::SoftCeiling {
                limit_microunits,
                currency: currency.clone(),
            })
        }
        _ => None,
    };
    let ledger = match aq_admission::transport::task_lineage_id(
        request.lineage.project_id,
        request.lineage.workspace_id,
        request.lineage.task_id,
    ) {
        Ok(lineage) => crate::observations::budget::lineage(state, lineage, soft.as_ref())
            .await
            .ok()
            .flatten()
            .map(|budget| budget.snapshot()),
        Err(_) => None,
    };
    let input = SelectionInputs {
        project: ProjectSnapshot {
            lineage: request.lineage.clone(),
            revision: request.project.project.revision,
            catalog_revision: request.project.catalog_activation_revision,
            catalog_digest: serde_json::from_value(json!(catalog.digest()))
                .map_err(|_| AppError::Internal)?,
            admission_snapshot_digest: digest(&request.project).map_err(|_| AppError::Internal)?,
            expires_at_ms: request.project.binding_observation.expires_at_ms,
            target: node.target.clone(),
            ceilings: ResourceCeilings {
                context_bytes: ceiling.context_bytes,
                wall_time_ms: ceiling.wall_time_ms,
                memory_bytes: ceiling.memory_bytes,
                cpu_millis: ceiling.cpu_millis,
                process_limit: ceiling.process_limit,
                cost_limit_microunits: ceiling.cost_limit_microunits,
                currency: ceiling.currency.clone(),
                require_hard_cost_enforcement: ceiling.require_hard_cost_enforcement,
            },
            allowed_environments: vec![request.project.environment.clone()],
            permission_ceiling: ceiling
                .permissions
                .iter()
                .map(|p| {
                    Ok(Scope {
                        operation: Text::new(&p.operation).map_err(|_| bad("project operation"))?,
                        resource: Text::new(&p.scope_reference)
                            .map_err(|_| bad("project scope"))?,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            agent_profile: request.project.agent_profile.clone(),
            intelligence_profile: request.project.intelligence_profile.clone(),
            execution_profile: request.project.execution_profile.clone(),
        },
        node,
        credentials,
        observations,
        registry_revision,
        // Legacy attempts are counted conservatively. No latest stop/retention/
        // reconciliation or provider ledger authority is invented for them.
        budget: BudgetSnapshot {
            lineage: request.lineage,
            revision: count.budget_revision(),
            attempts_started: count.started,
            exempt_attempts: replacement.as_ref().map_or(0, |r| r.exempt_attempts),
            last_attempt_id: replacement
                .as_ref()
                .map(|r| r.evidence.predecessor_attempt_id),
            elapsed_ms: match &replacement {
                Some(r) => Some(r.elapsed_ms),
                None if count.started == 0 => Some(0),
                None => None,
            },
            spent_microunits: ledger.as_ref().and_then(|l| l.spent_microunits),
            currency: ledger.as_ref().and_then(|l| l.currency.clone()),
            accounting: ledger
                .as_ref()
                .map_or(AccountingCompleteness::Unavailable, |l| l.accounting),
            accounting_source: ledger.as_ref().map_or_else(
                || artifact("aq.accounting.unavailable"),
                |l| l.accounting_source.clone(),
            ),
            hard_budget_provider: None,
            observed_at_ms: time,
            expires_at_ms: time
                .checked_add(OFFER_SELECTION_TTL_MS)
                .ok_or(AppError::Internal)?,
        },
        replacement: replacement.map(|r| r.evidence),
        estimates: vec![],
        compositions: request.compositions,
        now_ms: time,
        placement,
    };
    let outcome =
        select_for_offer(&catalog, &input).map_err(|_| bad("selection snapshot refused"))?;
    let decision = Decision {
        decision_id: Uuid::new_v4(),
        project: request.project,
        inputs: input,
        outcome,
    };
    let encoded = serde_json::to_value(&decision).map_err(|_| AppError::Internal)?;
    if serde_json::to_vec(&encoded)
        .map_err(|_| AppError::Internal)?
        .len()
        > 8 * 1024 * 1024
    {
        return Err(AppError::PayloadTooLarge("decision artifact bound".into()));
    }
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    // Metadata publication uses the project/node/catalog fence, then registry.
    fence_snapshot(state, &mut tx, &decision, false).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(712010)")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    subject.authorize(state, &mut tx).await?;
    if let Some(decision) = subject
        .lookup(&mut tx, request.operation_id, &payload)
        .await?
    {
        return Ok(decision);
    }
    if !request.record_refused && decision.outcome.resolved.is_none() {
        tx.rollback().await.map_err(db)?;
        return Ok(decision);
    }
    let row=sqlx::query("SELECT count(*) AS entries,COALESCE(sum(pg_column_size(decision)),0)::BIGINT AS bytes FROM intelligence_decisions").fetch_one(&mut *tx).await.map_err(db)?;
    let incoming: i64 = sqlx::query_scalar("SELECT pg_column_size($1::jsonb)::BIGINT")
        .bind(&encoded)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    if row.get::<i64, _>("entries") >= 1024
        || row
            .get::<i64, _>("bytes")
            .checked_add(incoming)
            .is_none_or(|n| n > 64 * 1024 * 1024)
    {
        return Err(conflict("decision artifact retention limit"));
    }
    subject
        .insert(&mut tx, request.operation_id, &payload, &decision, encoded)
        .await?;
    check_time(state, &decision)?;
    tx.commit().await.map_err(db)?;
    Ok(decision)
}

/// AQ-08 must call inside its SERIALIZABLE offer transaction, after resolving
/// current task/context authority, and retain all locks through atomic offer +
/// frozen selection insertion. Retry serialization failures; never do external
/// I/O here. This only checks facts, never inserts an attempt or grants execution.
pub async fn fence_for_offer(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    decision: &Decision,
) -> Result<()> {
    fence_retained_decision(state, tx, decision, None).await
}
/// Only the current locked, never-granted native reservation may be discounted
/// when comparing its original pre-offer decision. No durable count is changed.
pub(crate) async fn fence_for_preparation(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    decision: &Decision,
    binding: &aq_session_protocol::LaunchBinding,
) -> Result<()> {
    fence_owned_attempt(state, tx, decision, binding, None).await
}
/// Exact initial-grant receipt replay only; this never admits another attempt
/// or treats an issued grant as an ungranted reservation. The caller separately
/// verifies the immutable Prepare digest/receipt and original grant identity.
pub(crate) async fn fence_for_grant_replay(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    decision: &Decision,
    binding: &aq_session_protocol::LaunchBinding,
    grant_id: Uuid,
) -> Result<()> {
    fence_owned_attempt(state, tx, decision, binding, Some(grant_id)).await
}
async fn fence_owned_attempt(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    decision: &Decision,
    binding: &aq_session_protocol::LaunchBinding,
    grant_id: Option<Uuid>,
) -> Result<()> {
    use crate::execution::{
        authority::{self, Kind, Policy},
        profiled::Status,
    };
    let view = authority::load_locked(state, tx, &binding.session, Policy::IdentityOnly).await?;
    let selection = decision
        .outcome
        .resolved
        .as_ref()
        .ok_or_else(|| conflict("missing reserved selection"))?;
    if view.kind != Kind::ProfiledV1
        || !view.holds_task
        || view.revoked
        || view.stopped
        || view.cancellation_id.is_some()
        || match grant_id {
            None => {
                !matches!(view.status, Status::Offered | Status::Preparing)
                    || view.grant_id.is_some()
            }
            Some(id) => id.is_nil() || view.status != Status::Granted || view.grant_id != Some(id),
        }
        || view.offer_id != binding.offer_id
        || view.execution_spec_digest != binding.execution_spec_digest.as_str()
        || view.launch_manifest_digest.as_ref() != Some(&binding.launch_manifest_digest)
        || view.project_id != selection.lineage.project_id
        || view.task_id != selection.lineage.task_id
        || view.selection_digest.as_ref()
            != Some(&digest(selection).map_err(|_| AppError::Internal)?)
    {
        return Err(conflict("not the exact owned ungranted reservation"));
    }
    let link: Option<Uuid> = sqlx::query_scalar(
        "SELECT decision_id FROM profiled_launches WHERE attempt_id=$1 FOR SHARE",
    )
    .bind(binding.attempt_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?;
    if link != Some(decision.decision_id) {
        return Err(conflict("reservation decision mismatch"));
    }
    // Actual hot state is required: compatibility fixture grants cannot acquire
    // this capability by projecting grant=None from historical JSON.
    let hot: Option<Value> = sqlx::query_scalar(
        "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(binding.attempt_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?;
    let hot: crate::execution::authority::hot::HotState =
        serde_json::from_value(hot.ok_or_else(|| conflict("native reservation missing"))?)
            .map_err(|_| AppError::Internal)?;
    hot.validate_against(binding)
        .map_err(|_| conflict("invalid native reservation"))?;
    match grant_id {
        None if hot.grant.is_some()
            || !matches!(hot.status, Status::Offered | Status::Preparing) =>
        {
            return Err(conflict("reservation has issued authority"));
        }
        Some(id)
            if hot.status != Status::Granted
                || hot.grant.as_ref().is_none_or(|g| g.grant_id != id) =>
        {
            return Err(conflict("initial grant replay owner changed"));
        }
        _ => {}
    }
    fence_retained_decision(state, tx, decision, Some(binding.attempt_id)).await
}
async fn fence_retained_decision(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    decision: &Decision,
    owned_attempt: Option<Uuid>,
) -> Result<()> {
    let stored: Value = sqlx::query_scalar(
        "SELECT decision FROM intelligence_decisions WHERE decision_id=$1 FOR SHARE",
    )
    .bind(decision.decision_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?
    .ok_or_else(|| conflict("decision was never retained"))?;
    if digest(&stored).map_err(|_| AppError::Internal)?
        != digest(decision).map_err(|_| AppError::Internal)?
    {
        return Err(conflict(
            "decision differs from immutable retained artifact",
        ));
    }
    let isolation: String = sqlx::query_scalar("SHOW transaction_isolation")
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
    if isolation != "serializable" {
        return Err(conflict(
            "profiled admission requires serializable retry/budget predicate fencing",
        ));
    }
    if decision.outcome.resolved.is_none() {
        return Err(conflict("decision has no eligible selection"));
    }
    fence_snapshot(state, tx, decision, owned_attempt.is_some()).await?;
    // C4 placement is re-derived here, not only at selection: a node that
    // reached a ceiling or lost its template since selection is refused.
    crate::profiled_execution::placement::fence_locked(
        state,
        tx,
        decision,
        owned_attempt,
        now(state)?,
    )
    .await?;
    principal::fence(state, tx, decision).await?;
    check_time(state, decision)
}
async fn fence_snapshot(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    decision: &Decision,
    owned_attempt: bool,
) -> Result<()> {
    crate::project_bindings::fence_for_offer(state, tx, &decision.project).await?;
    registry::check_current(state, tx).await?;
    let current: Value =
        sqlx::query_scalar("SELECT state FROM execution_nodes WHERE node_id=$1 FOR SHARE")
            .bind(decision.project.node_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
    let ExecutionTarget::Worker { node_id, boot_id } = decision.inputs.node.target else {
        return Err(conflict(
            "central execution needs a distinct central authority adapter",
        ));
    };
    if node_id != decision.project.node_id || current["identity"]["boot_id"] != boot_id.to_string()
    {
        return Err(conflict("selection boot changed"));
    }
    let snapshot: Value = sqlx::query_scalar(
        "SELECT snapshot FROM intelligence_node_snapshots WHERE node_id=$1 FOR SHARE",
    )
    .bind(node_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    if digest(&snapshot).map_err(|_| AppError::Internal)?
        != digest(&decision.inputs.node).map_err(|_| AppError::Internal)?
    {
        return Err(conflict("installed capability snapshot changed"));
    }
    let credentials: Vec<_> = loaded(state)?
        .config
        .credentials
        .iter()
        .zip(&loaded(state)?.credentials)
        .filter(|(cfg, _)| cfg.allows(node_id))
        .map(|(_, c)| c.clone())
        .collect();
    if digest(&credentials).map_err(|_| AppError::Internal)?
        != digest(&decision.inputs.credentials).map_err(|_| AppError::Internal)?
    {
        return Err(conflict("credential authorization changed"));
    }
    let rows:Vec<Value>=sqlx::query_scalar("SELECT observation FROM intelligence_observations ORDER BY scope_source_digest LIMIT 1024 FOR SHARE").fetch_all(&mut **tx).await.map_err(db)?;
    let observations = rows
        .into_iter()
        .map(serde_json::from_value)
        .collect::<std::result::Result<Vec<AvailabilityObservation>, _>>()
        .map_err(|_| AppError::Internal)?;
    let mut expected = relevant(observations, &credentials, node_id);
    let (revision, at) = (decision.inputs.registry_revision, decision.inputs.now_ms);
    let owner = aq_machine_control::owner_attested(tx, &credentials, &expected, revision, at);
    expected.extend(owner.await?);
    if digest(&expected).map_err(|_| AppError::Internal)?
        != digest(&decision.inputs.observations).map_err(|_| AppError::Internal)?
    {
        return Err(conflict("relevant availability changed"));
    }
    let mut counts = crate::profiled_execution::no_grant::attempt_counts_locked(
        tx,
        decision.inputs.project.lineage.task_id,
    )
    .await?
    .check_retry_bound()?;
    if owned_attempt {
        counts.started = counts
            .started
            .checked_sub(1)
            .ok_or_else(|| conflict("reserved attempt count mismatch"))?;
        counts.history = counts
            .history
            .checked_sub(1)
            .ok_or_else(|| conflict("reserved history count mismatch"))?;
    }
    if counts.started != decision.inputs.budget.attempts_started
        || counts.budget_revision() != decision.inputs.budget.revision
    {
        return Err(conflict("task retry budget/history changed"));
    }
    check_time(state, decision)
}
pub(crate) fn check_time(state: &AppState, decision: &Decision) -> Result<()> {
    let time = now(state)?;
    if time < decision.inputs.now_ms
        || time >= decision.inputs.node.expires_at_ms
        || time >= decision.inputs.project.expires_at_ms
        || time >= decision.inputs.budget.expires_at_ms
        || decision
            .outcome
            .resolved
            .as_ref()
            .is_some_and(|r| time >= r.expires_at_ms)
    {
        return Err(conflict("selection expired during admission locks"));
    }
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExplainRequest {
    pub catalog_artifact_json: String,
    pub inputs: SelectionInputs,
}
pub(super) async fn explain(
    RequireOwner(_): RequireOwner,
    Json(request): Json<ExplainRequest>,
) -> Result<Json<Value>> {
    if request.catalog_artifact_json.len() > 2 * 1024 * 1024 {
        return Err(AppError::PayloadTooLarge("catalog byte bound".into()));
    }
    tokio::task::spawn_blocking(move || {
        let catalog =
            aq_profiles::ValidatedCatalog::from_artifact(request.catalog_artifact_json.as_bytes())
                .map_err(|_| bad("catalog artifact refused"))?;
        let outcome =
            select(&catalog, &request.inputs).map_err(|_| bad("selection snapshot refused"))?;
        let human = aq_selection::human_selection(&catalog, &request.inputs, &outcome)
            .map_err(|_| bad("human explanation refused"))?;
        Ok(Json(
            json!({"authority_admission":false,"snapshot_explanation":outcome,"selection":human}),
        ))
    })
    .await
    .map_err(|_| AppError::Internal)?
}

/// Current principal fence for already accepted no-launch input retrieval only.
/// Does not renew snapshots, select a candidate or authorize execution.
pub(crate) async fn fence_preparation_input_principal(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    attempt: Uuid,
) -> Result<()> {
    let row=sqlx::query("SELECT d.decision_id,d.decision#>>'{inputs,project,lineage,project_id}' AS project,l.launch_manifest#>>'{bindings,project_id}' AS bound_project FROM profiled_launches l JOIN intelligence_decisions d ON d.decision_id=l.decision_id WHERE l.attempt_id=$1 FOR SHARE OF l,d").bind(attempt).fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(||conflict("preparation principal missing"))?;
    let project: Uuid = row
        .get::<String, _>("project")
        .parse()
        .map_err(|_| AppError::Internal)?;
    let bound: Uuid = row
        .get::<String, _>("bound_project")
        .parse()
        .map_err(|_| AppError::Internal)?;
    principal::fence_subject(state, tx, row.get("decision_id"), project, bound).await
}

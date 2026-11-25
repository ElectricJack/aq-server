use super::*;

struct Registration<'a> {
    caller: &'a Caller,
    request: &'a wire::Request,
    registration: &'a wire::WaitRegistration,
}

pub(crate) async fn register_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    caller: &Caller,
    request: &wire::Request,
) -> Result<wire::Response> {
    if let Err(error) = request.validate() {
        return Ok(refused(request, contract_refusal(&error)));
    }
    let wire::Command::RegisterWait(registration) = &request.body.command else {
        return Ok(refused(request, wire::RefusalCode::InvalidRequest));
    };

    let admission = Registration {
        caller,
        request,
        registration,
    };
    if let Some(response) = admission.replay(tx).await? {
        return Ok(response);
    }
    let policy = default_policy();
    if let Err(code) = admission.authorize(&policy) {
        return Ok(refused(request, code));
    }
    if admission.taken(tx).await? {
        return Ok(refused(request, wire::RefusalCode::InvalidRequest));
    }
    let graph = known_graph(tx, caller.scope()).await?;
    let now = crate::agent_access::now(state);
    let plan = match admission.begin(policy, &graph, now) {
        Ok(plan) => plan,
        Err(code) => return Ok(refused(request, code)),
    };
    let (snapshot, effects) = SourceRounds {
        state,
        fence: &caller.fence,
        now,
    }
    .settle(tx, plan)
    .await?;
    if snapshot.boundaries().is_none() {
        // Central hold could not be installed against every source: nothing
        // is persisted and the caller may retry the same operation.
        return Ok(refused(request, wire::RefusalCode::SourceUnavailable));
    }
    let response = admission.insert(tx, &snapshot, now).await?;
    persist_effects(tx, registration.wait_id, &effects).await?;
    Ok(response)
}

impl Registration<'_> {
    async fn replay(&self, tx: &mut Transaction<'_, Postgres>) -> Result<Option<wire::Response>> {
        let Some(row) = sqlx::query(
        "SELECT snapshot,response FROM agent_waits WHERE caller_key=$1 AND operation_id=$2 FOR UPDATE",
    )
    .bind(&self.caller.key)
    .bind(self.request.body.operation_id)
    .fetch_optional(&mut **tx)
    .await?
    else { return Ok(None); };
        let snapshot = Snapshot::decode(&row.get::<Vec<u8>, _>("snapshot"))?;
        Ok(Some(
            match snapshot.replay_registration(self.request, &self.caller.fence) {
                Ok(_) => serde_json::from_value(row.get("response"))?,
                Err(engine::Error::Conflict) => {
                    refused(self.request, wire::RefusalCode::ChangedPayload)
                }
                Err(_) => refused(self.request, wire::RefusalCode::StaleGeneration),
            },
        ))
    }
    fn authorize(&self, policy: &ResolvedPolicy) -> std::result::Result<(), wire::RefusalCode> {
        if &self.registration.scope != self.caller.scope()
            || &self.registration.binding != self.caller.binding()
        {
            return Err(wire::RefusalCode::ScopeDenied);
        }
        if self.caller.fence.task_cancelled {
            return Err(wire::RefusalCode::StaleGeneration);
        }
        if !self
            .registration
            .predicate
            .members()
            .iter()
            .all(|member| supported(&member.condition))
        {
            return Err(wire::RefusalCode::UnsupportedCapability);
        }
        if let Err(error) = self
            .registration
            .reject_self(Some(self.request.body.operation_id), None)
        {
            return Err(contract_refusal(&error));
        }
        if self.registration.policy != policy.reference {
            return Err(wire::RefusalCode::InvalidRequest);
        }
        Ok(())
    }
    async fn taken(&self, tx: &mut Transaction<'_, Postgres>) -> Result<bool> {
        let taken: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM agent_waits WHERE wait_id=$1 OR (attempt_id=$2 AND phase<>'terminal') LIMIT 1",
    )
    .bind(self.registration.wait_id)
    .bind(self.registration.binding.attempt_id)
    .fetch_optional(&mut **tx)
    .await?;
        Ok(taken.is_some())
    }
    fn begin(
        &self,
        policy: ResolvedPolicy,
        graph: &KnownGraph,
        now: u64,
    ) -> std::result::Result<TransitionPlan, wire::RefusalCode> {
        let ids = StableIds {
            continuation_id: Uuid::new_v4(),
            outcome_id: Uuid::new_v4(),
            resume_intent_id: Uuid::new_v4(),
        };
        let context = engine::RegistrationContext {
            fence: &self.caller.fence,
            graph,
            now,
        };
        Snapshot::begin(self.request.clone(), policy, ids, context).map_err(|error| match error {
            engine::Error::Cycle | engine::Error::Capacity => wire::RefusalCode::KnownCycle,
            engine::Error::Stale => wire::RefusalCode::StaleGeneration,
            _ => wire::RefusalCode::InvalidRequest,
        })
    }
    async fn insert(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        snapshot: &Snapshot,
        now: u64,
    ) -> Result<wire::Response> {
        let response = respond(
            self.request,
            wire::ResponseBody::WaitRegistered(Box::new(snapshot.registered_wait()?)),
        );
        response.validate_against(self.request)?;

        sqlx::query("INSERT INTO agent_waits(wait_id,caller_key,operation_id,project_id,workspace_id,task_id,attempt_id,node_id,wait_generation,continuation_generation,predicate_digest,request_digest,phase,revision,next_due_ms,deadline_ms,created_at_ms,snapshot,response) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19)")
        .bind(self.registration.wait_id)
        .bind(&self.caller.key)
        .bind(self.request.body.operation_id)
        .bind(self.registration.scope.project_id)
        .bind(self.registration.scope.workspace_id)
        .bind(self.registration.binding.task_id)
        .bind(self.registration.binding.attempt_id)
        .bind(session_node(&self.registration.binding)?)
        .bind(i64_of(self.registration.wait_generation)?)
        .bind(i64_of(self.registration.continuation_generation)?)
        .bind(self.registration.predicate_digest.as_str())
        .bind(self.request.body_digest.as_str())
        .bind(phase_name(snapshot)?)
        .bind(i64_of(snapshot.revision())?)
        .bind(i64_of(row_due(snapshot, now))?)
        .bind(i64_of(snapshot.deadline_ms())?)
        .bind(i64_of(snapshot.original_created_at_ms())?)
        .bind(snapshot.encode()?)
        .bind(serde_json::to_value(&response)?)
        .execute(&mut **tx)
        .await?;
        Ok(response)
    }
}

/// Known component: the task edges of every active wait in the project. The
/// caller holds the per-project advisory lock, so the proof is serialized with
/// the registration write.
async fn known_graph(
    tx: &mut Transaction<'_, Postgres>,
    scope: &wire::Scope,
) -> Result<KnownGraph> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 76))")
        .bind(format!("agent-waits:{}", scope.project_id))
        .execute(&mut **tx)
        .await?;
    let rows = sqlx::query(
        "SELECT snapshot FROM agent_waits WHERE project_id=$1 AND phase<>'terminal' ORDER BY wait_id",
    )
    .bind(scope.project_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut edges = Vec::new();
    for row in rows {
        let snapshot = Snapshot::decode(&row.get::<Vec<u8>, _>("snapshot"))?;
        let registration = snapshot.registration();
        let owner = DependencyKey::Task {
            project: registration.binding.project_id,
            task: registration.binding.task_id,
        };
        for dependency in engine::receipts::task_dependencies(registration) {
            edges.push((owner.clone(), dependency));
        }
    }
    Ok(KnownGraph {
        scope: scope.clone(),
        complete_known_component: true,
        revision: artifact(
            "agent-wait-graph",
            wire::Digest::of(&serde_json::to_vec(&edges)?),
        ),
        edges,
        external_unknown: false,
    })
}

fn session_node(binding: &wire::Binding) -> Result<Uuid> {
    match serde_json::to_value(&binding.session)?["node_id"].as_str() {
        Some(node) => Uuid::parse_str(node).map_err(|_| WaitError("scope_denied")),
        None => Err(WaitError("scope_denied")),
    }
}
fn supported(condition: &wire::Condition) -> bool {
    matches!(
        condition,
        wire::Condition::Task { .. }
            | wire::Condition::Dependency { .. }
            | wire::Condition::Message { .. }
            | wire::Condition::HumanDecision { .. }
    )
}

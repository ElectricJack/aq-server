use super::*;
use aq_dashboard_contract::capabilities::capability;

/// X4: the mounted conversation and whether an active central session serves it.
async fn global_supervisor(pool: &sqlx::PgPool) -> Supervisor {
    let id: Option<Uuid> = sqlx::query_scalar(
        "SELECT conversation_id FROM global_supervisor_conversation WHERE singleton AND revision=1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .filter(|id: &Uuid| !id.is_nil());
    let Some(id) = id else {
        return Supervisor {
            status: "unconfigured".into(),
            conversation_id: None,
            reason: "The global supervisor conversation is not provisioned".into(),
        };
    };
    let bound = crate::agent_access::supervisor_conversation::dispatch::binding(pool, Some(id))
        .await
        .is_some();
    let ready = aq_supervisor_control::connected(pool, id).await;
    let (status, reason) = match (bound, ready) {
        (true, _) => ("available", "central_session_active"),
        (false, true) => ("available", "central_service_ready"),
        (false, false) => ("unavailable", "central_runtime_not_connected"),
    };
    Supervisor {
        status: status.into(),
        conversation_id: Some(id),
        reason: reason.into(),
    }
}
pub(in crate::dashboard_api) async fn bootstrap(
    request: &DashboardRequest,
    digest: Option<String>,
    mounted: MountedOwners,
) -> Result<Envelope<Bootstrap>> {
    let (state, runtime, user) = request.parts();
    let a = auth(state, runtime, user).await?;
    let global = if mounted.supervisor_conversation && a.owner {
        global_supervisor(&state.store.pool).await
    } else {
        Supervisor {
            status: "unconfigured".into(),
            conversation_id: None,
            reason: "The global supervisor is not connected yet".into(),
        }
    };
    let conversation = global.status == "available";
    let caps = CapabilitySources {
        state,
        mounted: &mounted,
        auth: &a,
        conversation,
    }
    .capabilities();
    let data = bootstrap_view(&a, digest, caps, global);
    let (pending, truncated) = pending_operations(runtime, &a).await?;
    fence(state, runtime, user, &a).await?;
    let mut response = envelope(
        state,
        Scope::AllAuthorized,
        data,
        truncated.then_some("pending_operations_truncated"),
    )?;
    response.pending_operations = pending;
    Ok(response)
}

async fn pending_operations(runtime: &Runtime, a: &Auth) -> Result<(Vec<Uuid>, bool)> {
    let mut tx = tx(runtime).await?;
    let mut pending: Vec<Uuid> = if a.owner {
        sqlx::query_scalar("SELECT operation_id FROM (SELECT operation_id,created_at FROM dashboard_operations WHERE principal_id=$1 AND retired_at IS NULL AND disposition IN ('pending','unknown') UNION ALL SELECT n.operation_id,to_timestamp(n.created_at_ms::double precision/1000) AS created_at FROM human_graph_operations n WHERE n.actor_id=$1 AND n.status->>'state' IN ('pending','unknown') AND NOT EXISTS(SELECT 1 FROM dashboard_operations d WHERE d.operation_id=n.operation_id)) pending ORDER BY created_at,operation_id LIMIT 101")
            .bind(a.actor).fetch_all(&mut *tx).await.map_err(|_|Error::Unavailable)?
    } else {
        vec![]
    };
    tx.commit().await.map_err(|_| Error::Unavailable)?;
    let truncated = pending.len() > 100;
    pending.truncate(100);
    Ok((pending, truncated))
}

fn bootstrap_view(
    a: &Auth,
    digest: Option<String>,
    caps: Vec<Capability>,
    global: Supervisor,
) -> Bootstrap {
    Bootstrap {
        principal: Principal {
            id: a.actor,
            display_name: a.email.clone(),
            authorization_revision: Revision(a.partition.clone()),
        },
        product: Product {
            version: env!("CARGO_PKG_VERSION").into(),
            dashboard_digest: digest.map(Digest),
        },
        contracts: vec![
            contract_ref(
                "dashboard-read",
                ContractVersion::Read1,
                contract::schema_bytes(),
            ),
            contract_ref(
                "dashboard-actions",
                ContractVersion::Actions1,
                crate::dashboard_api::actions::contract::schema_bytes(),
            ),
        ],
        capabilities: caps,
        limits: Limits {
            page_size: runtime::PAGE,
            max_page_size: runtime::MAX_PAGE,
            snapshot_members: runtime::MEMBERS as u16,
            snapshot_ttl_ms: runtime::TTL as u32,
        },
        default_scope: Scope::AllAuthorized,
        global_supervisor: global,
    }
}

fn contract_ref(name: &str, version: ContractVersion, bytes: Vec<u8>) -> ContractRef {
    ContractRef {
        name: name.into(),
        version,
        digest: Digest(format!("sha256:{}", hex::encode(Sha256::digest(bytes)))),
    }
}

struct CapabilitySources<'a> {
    state: &'a AppState,
    mounted: &'a MountedOwners,
    auth: &'a Auth,
    conversation: bool,
}
impl CapabilitySources<'_> {
    fn capabilities(&self) -> Vec<Capability> {
        let Self {
            state,
            mounted,
            auth: a,
            conversation: _,
        } = *self;
        let graph = mounted.graph && a.owner && state.agent_access.is_some();
        let mut caps = self.dashboard();
        aq_dashboard_contract::capabilities::GraphCapabilities {
            read: graph,
            comments: graph,
            mutate: graph && crate::agent_access::human::task_create_available(state),
        }
        .append_to(&mut caps);
        caps.extend(self.decisions());
        caps.extend(aq_dashboard_contract::capabilities::providers(
            a.owner,
            a.owner && state.config.intelligence.is_some(),
        ));
        caps.extend(self.collaboration());
        caps.extend(
            aq_dashboard_contract::capabilities::ResultSources {
                results: mounted.results,
                media_read: mounted.media_read,
                media_upload: mounted.media_upload,
            }
            .capabilities(a.owner),
        );
        let playbooks = a.owner && mounted.playbook_owner == Some(a.actor);
        caps.extend(aq_dashboard_contract::capabilities::playbook_capabilities(
            playbooks,
            playbooks && mounted.playbook_loaded_definitions,
            playbooks
                && mounted
                    .playbook_catalog
                    .as_ref()
                    .is_some_and(|support| support.available()),
        ));
        caps
    }
    fn dashboard(&self) -> Vec<Capability> {
        let a = self.auth;
        let mounted = self.mounted;
        vec![
            capability("dashboard.overview", true, ""),
            capability(
                "tasks.workspace.resources",
                a.owner,
                "Task workspace resources require an owner account",
            ),
            capability(
                "tasks.route.preferences",
                a.owner,
                "Task route preferences require an owner account",
            ),
            capability(
                "performance.metrics.v1",
                a.owner,
                "Performance requires project inspection access",
            ),
            capability(
                "sessions.read",
                a.visibility_complete,
                "Session inspection is unavailable for this account",
            ),
            capability(
                "session.usage.read",
                mounted.session_usage && a.visibility_complete,
                "Session usage inspection requires a mounted observation owner and bounded session visibility",
            ),
            capability(
                "session.events.read",
                mounted.session_events && a.visibility_complete,
                "Session event inspection requires a mounted observation owner and bounded session visibility",
            ),
            capability(
                "projects.read",
                a.owner,
                "Project inspection requires an owner account",
            ),
            capability("status.read", true, ""),
            capability("workers.read", true, ""),
            capability(
                "notifications.read",
                mounted.notifications,
                "Notification inspection requires a mounted notification owner",
            ),
        ]
    }
    fn decisions(&self) -> [Capability; 6] {
        let a = self.auth;
        let mounted = self.mounted;
        let reviews = a.owner && mounted.reviews;
        let questions = mounted.questions && a.owner;
        [
            capability(
                "search.read",
                a.visibility_complete,
                "Session visibility exceeds the bounded authorization partition",
            ),
            capability(
                "decisions.read",
                reviews,
                "Decision inspection requires an owner account and a mounted review owner",
            ),
            capability(
                "playbooks.decisions",
                a.owner && mounted.playbook_decisions,
                "Playbook decisions require an owner account and a mounted host playbook namespace",
            ),
            capability(
                "decisions.resolve",
                reviews,
                "Decision resolution requires an owner account and a mounted review owner",
            ),
            capability(
                "questions.read",
                questions,
                "Agent question inspection requires an owner account",
            ),
            capability(
                "questions.answer",
                // `question.answer` is a dashboard-actions-1 kind, gated with `dashboard.actions`.
                questions,
                "Answering agent questions requires an owner account and dashboard actions",
            ),
        ]
    }
    fn collaboration(&self) -> [Capability; 10] {
        let Self {
            auth: a,
            mounted,
            state,
            conversation,
        } = *self;
        let supervisor = mounted.supervisor_conversation && a.owner;
        [
            capability(
                "supervisor.history.read",
                supervisor,
                "Global supervisor history requires an owner account and mounted conversation owner",
            ),
            capability(
                "supervisor.message.enqueue",
                supervisor,
                "Global supervisor enqueue requires an owner account and mounted conversation owner",
            ),
            capability(
                "session.message.enqueue",
                a.owner,
                "Session messages require an owner account",
            ),
            capability(
                "session.nudge",
                a.owner,
                "Session nudges require an owner account",
            ),
            capability(
                "session.operations.read",
                mounted.session_operations && a.owner && a.visibility_complete,
                "Session operations require an owner account",
            ),
            capability(
                "waits.read",
                mounted.waits && a.owner,
                "Wait inspection requires an owner account",
            ),
            capability(
                "supervisor.conversation",
                conversation,
                "The global supervisor is not connected yet",
            ),
            capability(
                "terminal.watch",
                state.config.terminals.is_some() && a.visibility_complete,
                "Terminal relay or session inspection is unavailable",
            ),
            capability(
                "terminal.control",
                state.config.terminals.is_some() && a.owner,
                "Terminal control requires an owner account and configured terminal relay",
            ),
            capability(
                "dashboard.actions",
                a.owner,
                "Dashboard actions require an owner account",
            ),
        ]
    }
}

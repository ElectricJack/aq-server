use super::*;

/// F1b plan authority: the plan and its startup bytes must be the recorded turn.
struct Dispatched;
#[async_trait::async_trait]
impl PlanAuthority for Dispatched {
    async fn authorize(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        central: &CentralPlan,
        host: &HostPlan,
    ) -> Result<()> {
        aq_server::agent_access::supervisor_conversation::dispatch::authorize_dispatch(
            tx,
            central,
            &host.startup.payload.digest,
        )
        .await
        .map_err(|e| anyhow::anyhow!("dispatch refused: {e:?}"))
    }
}
#[path = "conversation/http.rs"]
mod http;
use aq_server::agent_access::supervisor_conversation::{
    self as conversation, dispatch, reply, reservations::*,
};
use axum::http::StatusCode;
use chrono::TimeZone;
use http::{Client, assert_history_pages_exactly, conversation_schema};
use serde_json::json;
const BODY: &str = "Hello supervisor ☃\nPlease plan the next milestone.";
const AS_OWNER: [(&str, &str); 2] = [("x-aq-dev-email", "owner@localhost"), ("x-aq-request", "1")];
const REPLY_PATH: &str = "/v1/supervisor/conversation/replies";
struct Api {
    state: aq_server::AppState,
    owner_app: axum::Router,
    supervisor_app: axum::Router,
    user: aq_server::store::User,
}
struct Mail {
    message_id: Uuid,
    conversation_id: Uuid,
    messages: String,
}
struct Reservation {
    id: Uuid,
    actor: Uuid,
}
struct Prepared {
    central: CentralPlan,
    turn: dispatch::Turn,
    session_id: Uuid,
}
struct Session {
    case: Case,
    host: Supervisor<Dispatched, FixtureAdapter>,
    turn: dispatch::Turn,
    actor: Uuid,
    session_id: Uuid,
    directory: std::path::PathBuf,
}
struct Credential {
    token: String,
    installed: String,
    _home: tempfile::TempDir,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_message_reaches_supervisor_and_reply_is_recorded() {
    let db = aq_server::test_support::TestDb::create().await;
    let api = Api::new(&db).await;
    let mail = api.enqueue().await;
    let reserved = api.reserve(&mail).await;
    let prepared = reserved.prepare(&db, &mail).await;
    let session = prepared.start(db, reserved.actor).await;
    api.assert_pending(&mail).await;
    session.verify_handoff_fence().await;
    let (path, token) = session.activate().await;
    let credential = session.credential(path, token).await;
    let bearer = format!("Bearer {}", credential.installed);
    let headers = [("authorization", bearer.as_str())];
    let client = Client {
        app: &api.supervisor_app,
        headers: &headers,
    };
    let reply = session.reply(client).await;
    api.verify_history(&session, &mail, reply).await;
    api.more_replies(&mail, (reply, session.turn.dispatch_id), client)
        .await;
    session.verify_stop(client, &credential.token).await;
}

impl Api {
    fn owner(&self) -> Client<'_> {
        Client {
            app: &self.owner_app,
            headers: &AS_OWNER,
        }
    }
    async fn new(db: &aq_server::test_support::TestDb) -> Self {
        let store = db.store().await;
        let now = chrono::Utc.timestamp_millis_opt(1_000_000).unwrap();
        let user = store
            .bootstrap_owner("owner@localhost", "dev", "owner@localhost", now)
            .await
            .unwrap()
            .unwrap();
        let config: aq_server::config::Config = toml::from_str("format_version=1\nenvironment='development'\n[server]\nadmin_bind='127.0.0.1:0'\nworker_bind='127.0.0.1:0'\npublic_origin='http://127.0.0.1:8080'\n[auth]\nmode='dev'\ndev_email='owner@localhost'\n[bootstrap]\nowner_email='owner@localhost'\nenabled=true\n").unwrap();
        let state = aq_server::AppState {
            internal_work: Default::default(),
            store,
            identity: aq_server::identity::verifier_from_config(
                &config.auth,
                reqwest::Client::new(),
            ),
            config: Arc::new(config),
            clock: Arc::new(move || now),
            agent_access: None,
            source_retention: None,
        };
        let owner_app = conversation::routes(&state)
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                aq_server::authz::csrf_guard,
            ))
            .with_state(state.clone());
        let supervisor_app = conversation::supervisor_routes(&state).with_state(state.clone());
        Self {
            state,
            user,
            owner_app,
            supervisor_app,
        }
    }
    async fn enqueue(&self) -> Mail {
        // 1. The owner sends one message through the mounted owner route.
        let (c, found) = self
            .owner()
            .call("GET", "/api/human/conversations/global", None)
            .await;
        assert_eq!(c, StatusCode::OK, "{found}");
        let conversation_id: Uuid = found["conversation"]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let messages = format!("/api/human/conversations/{conversation_id}/messages");
        let message_id = Uuid::new_v4();
        let create = json!({"operation_id":Uuid::new_v4(),"message_id":message_id,"expected_conversation_revision":"1","body":BODY,"context":[],"attachments":[]});
        let (c, queued) = self
            .owner()
            .call("POST", &messages, Some(create.to_string()))
            .await;
        assert_eq!(c, StatusCode::OK, "{queued}");
        assert_eq!(queued["state"], "queued");
        Mail {
            message_id,
            conversation_id,
            messages,
        }
    }
    async fn reserve(&self, mail: &Mail) -> Reservation {
        let conversation_id = mail.conversation_id;
        let user = &self.user;
        // 2. Reserve it as one bounded batch for the configured supervisor actor.
        let actor = Uuid::new_v4();
        let owner = ReservationOwner::new(&self.state);
        owner
            .configure_actor(
                user,
                &serde_json::to_vec(&Configure {
                    operation_id: Uuid::new_v4(),
                    actor_id: actor,
                    expected_actor_revision: 0,
                    policy: LocalPolicy {
                        schema_version: 1,
                        kind: PolicyKind::GlobalSupervisorReservation,
                        actor_id: actor,
                        conversation_id,
                        enabled: true,
                    },
                })
                .unwrap(),
            )
            .await
            .unwrap();
        let revision = owner.inspect_actor(user, actor).await.unwrap().revision;
        let preview = owner
            .preview_batch(
                user,
                PreviewRequest {
                    actor_id: actor,
                    expected_actor_revision: revision,
                    scan_after: 0,
                    through_cursor: None,
                },
            )
            .await
            .unwrap();
        let reserved = owner
            .reserve_batch(
                user,
                &serde_json::to_vec(&Reserve {
                    operation_id: Uuid::new_v4(),
                    reservation_id: Uuid::new_v4(),
                    actor_id: actor,
                    expected_actor_revision: preview.actor_revision,
                    expected_policy_revision: preview.policy_revision,
                    scan_after: preview.scan_after,
                    through_cursor: preview.through_cursor,
                    selection_digest: preview.selection_digest.clone(),
                })
                .unwrap(),
            )
            .await
            .unwrap()
            .reservation
            .unwrap();
        Reservation {
            id: reserved.id,
            actor,
        }
    }
    async fn assert_pending(&self, mail: &Mail) {
        // Host Running is not delivery: until the handoff proof the owner sees pending.
        let (_, history) = self.owner().call("GET", &mail.messages, None).await;
        assert_eq!(history["items"][0]["host_delivery"], "pending", "{history}");
    }
    async fn verify_history(&self, session: &Session, mail: &Mail, reply_id: Uuid) {
        let message_id = mail.message_id;
        let turn = &session.turn;
        // 8. The conversation shows the delivered owner message, then the reply.
        let (c, history) = self.owner().call("GET", &mail.messages, None).await;
        assert_eq!(c, StatusCode::OK, "{history}");
        conversation_schema(&history);
        let items = history["items"].as_array().unwrap();
        assert_eq!(items.len(), 2, "{history}");
        assert_eq!(items[0]["message_id"], json!(message_id));
        assert_eq!(items[0]["host_delivery"], "delivered");
        assert_eq!(items[1]["message_id"], json!(reply_id));
        assert_eq!(items[1]["author"]["kind"], "supervisor");
        assert_eq!(items[1]["dispatch"]["dispatch_id"], json!(turn.dispatch_id));
        assert_eq!(
            items[1]["dispatch"]["host_operation_id"],
            json!(turn.host_operation_id)
        );
    }
    async fn more_replies(&self, mail: &Mail, receipt: (Uuid, Uuid), client: Client<'_>) {
        let (reply_id, dispatch_id) = receipt;
        // 8b. Many replies to one turn: each has its own cursor and pages exactly.
        let mut expected = vec![mail.message_id, reply_id];
        for n in 0..17 {
            let id = Uuid::new_v4();
            let more = json!({"operation_id":Uuid::new_v4(),"message_id":id,"dispatch_id":dispatch_id,"body":format!("Reply part {n}")}).to_string();
            let (c, r) = client.call("POST", REPLY_PATH, Some(more)).await;
            assert_eq!(c, StatusCode::OK, "{r}");
            expected.push(id);
        }
        assert_history_pages_exactly(&self.owner_app, &mail.messages, &AS_OWNER, &expected).await;
    }
}

impl Reservation {
    async fn prepare(&self, db: &aq_server::test_support::TestDb, mail: &Mail) -> Prepared {
        let (actor, conversation_id, message_id) =
            (self.actor, mail.conversation_id, mail.message_id);
        // 3. Record the turn (dispatch intent + exact host input) before any handoff.
        let mut central = example_plan();
        let CentralSubject::CentralConversation {
            actor_id,
            conversation_id: subject_conversation,
            ..
        } = &mut central.subject;
        *actor_id = actor;
        *subject_conversation = conversation_id;
        let SessionIdentity::Central { session_id, .. } = central.session else {
            panic!("central session")
        };
        let pool = db.pool().await;
        let turn = dispatch::prepare_turn(
            &pool,
            actor,
            self.id,
            central.generation,
            session_id,
            1_000_000,
        )
        .await
        .unwrap();
        assert_eq!(turn.batch.messages.len(), 1);
        assert_eq!(turn.batch.messages[0].message_id, message_id);
        assert!(
            String::from_utf8(turn.input.clone())
                .unwrap()
                .contains(BODY)
        );
        let replayed = dispatch::prepare_turn(
            &pool,
            actor,
            self.id,
            central.generation,
            session_id,
            1_000_001,
        )
        .await
        .unwrap();
        assert_eq!(
            replayed, turn,
            "the recorded turn is replayed, never re-rendered"
        );
        central.dispatch_id = turn.dispatch_id;
        central.host_operation_id = turn.host_operation_id;
        central.batch = turn.batch.clone();
        let plan_wire = serde_json::to_string(&central).unwrap();
        assert!(
            !plan_wire.contains("Please plan"),
            "plans carry refs, never bodies"
        );
        Prepared {
            central,
            turn,
            session_id,
        }
    }
}

impl Prepared {
    async fn start(self, db: aq_server::test_support::TestDb, actor: Uuid) -> Session {
        let Prepared {
            central,
            turn,
            session_id,
        } = self;
        // 4. Start the F1a session on that exact plan; the host stages the turn.
        let case = Case::build(db, central, turn.input.clone()).await;
        assert!(!dispatch::live(&case.pool).await);
        let host = case.controller_with(Dispatched);
        let started = case.start(&host).await;
        assert_eq!(started.run.phase(), Phase::RunningReported);
        assert!(dispatch::live(&case.pool).await);
        let observed = host.observe(actor).await.unwrap();
        let directory = case
            .root
            .path()
            .join(observed.guardian.intent.launch_id.to_string());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            std::fs::read(directory.join("delivered")).unwrap(),
            turn.input
        );
        Session {
            case,
            host,
            directory,
            actor,
            turn,
            session_id,
        }
    }
}

impl Session {
    async fn verify_handoff_fence(&self) {
        let Self {
            case,
            host,
            directory,
            turn,
            ..
        } = self;
        let actor = self.actor;
        // Uncertain handoff must not issue a credential or start native reply access.
        let staged_input = directory.join("context-0.bin");
        std::fs::write(&staged_input, b"mismatched input").unwrap();
        assert!(host.enable_replies(actor, 998_999).await.is_err());
        assert!(!directory.join("supervisor.credential").exists());
        let issued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM central_supervisor_credentials WHERE actor_id=$1",
        )
        .bind(actor)
        .fetch_one(&case.pool)
        .await
        .unwrap();
        assert_eq!(issued, 0);
        assert!(matches!(
            host.inspect(actor).await.unwrap().run.dispatch(),
            DispatchState::Unknown { .. }
        ));
        std::fs::write(&staged_input, &turn.input).unwrap();
    }
    async fn activate(&self) -> (std::path::PathBuf, String) {
        let Self {
            host, directory, ..
        } = self;
        let actor = self.actor;
        // 5. Use the binary's activation path: verified handoff, issue and install.
        let path = host.enable_replies(actor, 999_000).await.unwrap();
        let handed = host.inspect(actor).await.unwrap();
        let DispatchState::Resolved { evidence } = handed.run.dispatch() else {
            panic!("handoff resolved: {:?}", handed.run.dispatch())
        };
        assert_eq!(evidence.disposition, HandoffDisposition::HandedOff);
        assert_eq!(
            host.handoff(actor).await.unwrap().run.dispatch(),
            handed.run.dispatch()
        );
        // 6. One scoped credential, bound to this generation, only beside the session.
        assert!(path.starts_with(directory));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let token = std::fs::read_to_string(&path).unwrap();
        (path, token)
    }
    async fn credential(&self, path: std::path::PathBuf, token: String) -> Credential {
        let case = &self.case;
        let actor = self.actor;
        // Controller re-adoption must not revoke the token held by the native child.
        let resumed = case.controller_with(Dispatched);
        assert_eq!(resumed.enable_replies(actor, 999_001).await.unwrap(), path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), token);
        let issued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM central_supervisor_credentials WHERE actor_id=$1",
        )
        .bind(actor)
        .fetch_one(&case.pool)
        .await
        .unwrap();
        assert_eq!(issued, 1, "restart reuses the installed credential");
        let lifetime: i64 = sqlx::query_scalar(
        "SELECT expires_at_ms-issued_at_ms FROM central_supervisor_credentials WHERE actor_id=$1",
    )
    .bind(actor)
    .fetch_one(&case.pool)
    .await
    .unwrap();
        assert!(lifetime > 0 && lifetime <= 60_000);
        // The root wrapper stages a 0600 supervisor-home copy before native spawn.
        let home = tempfile::tempdir().unwrap();
        let native_path = aq_supervisor_host::credentials::stage_supervisor_credential(
            &path,
            home.path(),
            case.host.deadline_boot_ms,
        )
        .await
        .unwrap();
        let installed = std::fs::read_to_string(native_path).unwrap();
        assert_eq!(installed, token);
        let principal = reply::authenticate(&case.pool, &installed, "messaging", 1_000_000)
            .await
            .unwrap();
        assert_eq!(principal.actor_id, actor);
        assert_eq!(principal.generation, 1);
        assert_eq!(principal.scopes, reply::SCOPES.map(String::from).to_vec());
        Credential {
            token,
            installed,
            _home: home,
        }
    }
    async fn reply(&self, client: Client<'_>) -> Uuid {
        let Self {
            turn,
            actor,
            session_id,
            ..
        } = self;
        let reply_id = Uuid::new_v4();
        let request = json!({"operation_id":Uuid::new_v4(),"message_id":reply_id,"dispatch_id":turn.dispatch_id,"body":"Supervisor reply: milestone plan follows."}).to_string();
        let (c, receipt) = client.call("POST", REPLY_PATH, Some(request.clone())).await;
        assert_eq!(c, StatusCode::OK, "{receipt}");
        assert_eq!(receipt["dispatch_id"], json!(turn.dispatch_id));
        assert_eq!(
            receipt["author"],
            json!({"kind":"central_supervisor","actor_id":actor,"generation":1,"session_id":session_id})
        );
        assert_eq!(receipt["logical_admission"], "queued");
        let (c, again) = client.call("POST", REPLY_PATH, Some(request)).await;
        assert_eq!((c, &again), (StatusCode::OK, &receipt), "reply replays");
        let wrong = json!({"operation_id":Uuid::new_v4(),"message_id":Uuid::new_v4(),"dispatch_id":Uuid::new_v4(),"body":"wrong turn"}).to_string();
        let (c, _) = client.call("POST", REPLY_PATH, Some(wrong)).await;
        assert_eq!(c, StatusCode::CONFLICT);
        let (c, _) = Client {
            app: client.app,
            headers: &AS_OWNER,
        }
        .call("POST", REPLY_PATH, Some("{}".into()))
        .await;
        assert_eq!(
            c,
            StatusCode::UNAUTHORIZED,
            "owner sessions cannot author replies"
        );
        reply_id
    }
    async fn verify_stop(&self, client: Client<'_>, token: &str) {
        let Self {
            case, host, turn, ..
        } = self;
        let actor = self.actor;
        // 9. Stopping the generation ends the credential and the live capability.
        let stopped = host.stop(actor).await.unwrap();
        assert_eq!(stopped.run.phase(), Phase::StoppedReported);
        assert!(host.enable_replies(actor, 1_000_000).await.is_err());
        assert!(!dispatch::live(&case.pool).await);
        let late = json!({"operation_id":Uuid::new_v4(),"message_id":Uuid::new_v4(),"dispatch_id":turn.dispatch_id,"body":"too late"}).to_string();
        let (c, _) = client.call("POST", REPLY_PATH, Some(late)).await;
        assert_eq!(c, StatusCode::UNAUTHORIZED);
        let leaked: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM central_supervisor_credentials c WHERE strpos(to_jsonb(c)::text,$1)>0)+(SELECT count(*) FROM central_host_facts f WHERE strpos(to_jsonb(f)::text,$1)>0)+(SELECT count(*) FROM mailbox_messages m WHERE strpos(to_jsonb(m)::text,$1)>0)")
        .bind(token)
        .fetch_one(&case.pool)
        .await
        .unwrap();
        assert_eq!(
            leaked, 0,
            "the plaintext credential is never stored centrally"
        );
    }
}

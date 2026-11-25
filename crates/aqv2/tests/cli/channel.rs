//! Channel mode (spec 0006 §4): the CLI over a fake Unix-socket channel.
//!
//! - Every command in `protocol/commands/v1/commands.json` is reachable
//!   through `run`, and through its CLI tokens unless a typed subcommand
//!   owns them. The list is read from the file, so a new command is covered
//!   without editing this test.
//! - Golden files pin each typed subcommand's request (both modes) and the
//!   human renderings. Regenerate them (and review the diff) with
//!   `AQ_REGENERATE_GOLDEN=1 cargo test -p aqv2 --test cli -- channel::typed_subcommands_build_the_golden_requests channel::human_renderings_match_golden`.
//!   The reachability test reads `typed.json`, so run it again afterwards.
use super::{Seen, server};
use axum::{
    Router,
    body::to_bytes,
    extract::{Request, State},
    http::StatusCode,
    response::IntoResponse,
    routing::any,
};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;

const TASK: &str = "00000000-0000-4000-8000-0000000000a1";
const PROJECT: &str = "00000000-0000-4000-8000-0000000000a2";
const ATTEMPT: &str = "00000000-0000-4000-8000-0000000000a3";
const SESSION: &str = "00000000-0000-4000-8000-0000000000a4";
const WORKSPACE: &str = "00000000-0000-4000-8000-0000000000a5";
const OTHER_TASK: &str = "00000000-0000-4000-8000-0000000000b1";
const RECORD: &str = "00000000-0000-4000-8000-0000000000b2";
const COMMENT: &str = "00000000-0000-4000-8000-0000000000b3";
const DECISION: &str = "00000000-0000-4000-8000-0000000000b4";
const WAIT: &str = "00000000-0000-4000-8000-0000000000b5";
const MESSAGE: &str = "00000000-0000-4000-8000-0000000000b6";
const OPERATION: &str = "00000000-0000-4000-8000-0000000000b7";
const CONVERSATION: &str = "00000000-0000-4000-8000-0000000000b8";

fn repo(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}
fn golden(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name)
}
fn regenerate() -> bool {
    std::env::var_os("AQ_REGENERATE_GOLDEN").is_some()
}
fn check_golden(path: PathBuf, actual: &str) {
    let name = path.file_name().unwrap().to_string_lossy();
    if regenerate() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        expected == actual,
        "{name} drifted; regenerate with AQ_REGENERATE_GOLDEN=1 and review the diff.\n--- actual ---\n{actual}"
    );
}
fn identity() -> Value {
    json!({"api_version":1,"principal":"agent","attempt_id":ATTEMPT,"task_id":TASK,"project_id":PROJECT,
        "session_id":SESSION,"session_generation":3,"workspace_id":WORKSPACE,
        "supervisor_conversation_id":CONVERSATION,"profile_id":"aq.coding",
        "grants":[{"operation":"comment.write","scope":"task:subtree"},
                  {"operation":"task.create.subtask","scope":"task:subtree","limits":{"max_created_tasks":5,"max_depth":2}},
                  {"operation":"task.read","scope":"project:self"}],
        "grants_digest":format!("sha256:{}", "0".repeat(64)),
        "credential":{"version":"1","expires_at":"2026-11-06T12:00:00+00:00","authority_revision":"1"}})
}
fn fixture(name: &str) -> Value {
    serde_json::from_slice(
        &std::fs::read(repo("protocol/commands/v1/fixtures").join(name)).unwrap(),
    )
    .unwrap()
}

type Reply = Arc<dyn Fn(&str, &str, &Value) -> (StatusCode, Value) + Send + Sync>;
#[derive(Clone)]
struct FakeState {
    reply: Reply,
    seen: mpsc::UnboundedSender<Seen>,
}
async fn handle(State(state): State<FakeState>, request: Request) -> impl IntoResponse {
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, 65537).await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    let path = parts.uri.to_string();
    let reply = (state.reply)(parts.method.as_str(), &path, &body);
    state
        .seen
        .send(Seen {
            method: parts.method.to_string(),
            path,
            headers: parts.headers,
            body,
        })
        .unwrap();
    (reply.0, axum::Json(reply.1))
}
/// A fake channel: a Unix socket served by axum, standing in for the proxy.
struct Fake {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    seen: mpsc::UnboundedReceiver<Seen>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fake {
    fn drain(&mut self) -> Vec<Seen> {
        let mut out = vec![];
        while let Ok(s) = self.seen.try_recv() {
            out.push(s);
        }
        out
    }
    async fn run(&self, args: &[&str]) -> aqv2::Result<Value> {
        aqv2::execute_channel(&self.socket, args).await
    }
}
/// The default channel: `whoami` answers with the agent identity, discovery
/// with an empty listing, and every other command with `{"ok":true}`.
fn default_reply(method: &str, path: &str, _: &Value) -> (StatusCode, Value) {
    match (method, path) {
        ("POST", "/api/v1/commands/whoami") => (StatusCode::OK, identity()),
        ("GET", p) if p.starts_with("/api/v1/commands") => (
            StatusCode::OK,
            json!({"api_version":1,"principal":"agent","commands":[],"unavailable":[]}),
        ),
        _ => (StatusCode::OK, json!({"api_version":1,"ok":true})),
    }
}
async fn fake(
    reply: impl Fn(&str, &str, &Value) -> (StatusCode, Value) + Send + Sync + 'static,
) -> Fake {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("aq.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let app = Router::new().fallback(any(handle)).with_state(FakeState {
        reply: Arc::new(reply),
        seen: tx,
    });
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Fake {
        _dir: dir,
        socket,
        seen: rx,
        task,
    }
}
fn assert_no_credential(seen: &Seen) {
    for header in [
        "authorization",
        "cf-access-jwt-assertion",
        "x-aq-dev-email",
        "origin",
    ] {
        assert!(
            !seen.headers.contains_key(header),
            "{header} sent on the channel"
        );
    }
}
fn uuid(v: &Value) -> Option<uuid::Uuid> {
    v.as_str()?.parse().ok()
}

// ---- every command is reachable -----------------------------------------

/// Command names the typed golden cases cover (their CLI tokens are typed).
fn typed_names() -> BTreeSet<String> {
    let golden: Value =
        serde_json::from_str(&std::fs::read_to_string(golden("typed.json")).unwrap()).unwrap();
    golden
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|c| c["calls"].as_array().unwrap().iter())
        .filter_map(|call| {
            call["path"]
                .as_str()?
                .strip_prefix("/api/v1/commands/")
                .map(String::from)
        })
        .collect()
}
#[tokio::test]
async fn every_command_in_commands_json_is_reachable_through_run() {
    let doc: Value =
        serde_json::from_slice(&std::fs::read(repo("protocol/commands/v1/commands.json")).unwrap())
            .unwrap();
    let commands = doc["commands"].as_array().unwrap();
    assert!(!commands.is_empty());
    // The binary's built-in catalog is this same file.
    assert_eq!(aqv2::catalog::commands(), commands.as_slice());
    let typed = typed_names();
    let mut fake = fake(default_reply).await;
    for c in commands {
        let name = c["name"].as_str().unwrap();
        let result = fake.run(&["run", name]).await;
        let path = format!("/api/v1/commands/{name}");
        assert_eq!(
            result.unwrap(),
            default_reply("POST", &path, &json!({})).1,
            "{name}"
        );
        let seen = fake.drain();
        assert_eq!(seen.len(), 1, "{name}");
        let s = &seen[0];
        assert_eq!(
            (s.method.as_str(), s.path.as_str()),
            ("POST", &*format!("/api/v1/commands/{name}"))
        );
        assert_no_credential(s);
        assert!(s.body["params"].is_object(), "{name}");
        let mutation = c["kind"] == "mutation";
        assert_eq!(uuid(&s.body["operation_id"]).is_some(), mutation, "{name}");
        let inline = c["principals"]["agent"]["params_schema"]["properties"]
            .get("operation_id")
            .is_some();
        if inline || aqv2::catalog::takes_operation_id(name, "agent") {
            assert_eq!(
                s.body["params"]["operation_id"], s.body["operation_id"],
                "{name}"
            );
        } else {
            assert!(s.body["params"].get("operation_id").is_none(), "{name}");
        }

        // The command's CLI tokens reach it too, unless a typed subcommand
        // owns them (and then a typed golden case covers the command).
        let tokens: Vec<&str> = c["cli"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap())
            .collect();
        let by_tokens = fake.run(&tokens).await;
        let seen = fake.drain();
        let reached = by_tokens.is_ok()
            && seen
                .last()
                .is_some_and(|s| s.path == format!("/api/v1/commands/{name}"));
        assert!(
            reached || typed.contains(name),
            "`{}` does not reach {name} and no typed golden case covers it",
            tokens.join(" ")
        );
    }
    // Typed coverage names only registered commands.
    let names: BTreeSet<String> = commands
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        typed.is_subset(&names),
        "{:?}",
        typed.difference(&names).collect::<Vec<_>>()
    );
}
// A command registered after this binary was built: `run` sends it as a
// possible mutation, and its CLI tokens resolve through discovery.
#[tokio::test]
async fn a_command_unknown_to_this_binary_is_still_reachable() {
    let mut fake = fake(|method, path, body| match (method, path) {
        ("GET", "/api/v1/commands?all=true") => (
            StatusCode::OK,
            json!({"api_version":1,"principal":"agent","commands":[{"name":"future.thing.do","kind":"mutation",
                "summary":"Later","cli":["future","thing","do"],"grant":"future.do","scope":"task:self"}],"unavailable":[]}),
        ),
        _ => default_reply(method, path, body),
    })
    .await;
    fake.run(&[
        "run",
        "future.thing.do",
        "--param",
        "x=1",
        "--param",
        "n:=2",
    ])
    .await
    .unwrap();
    let s = fake.drain().pop().unwrap();
    assert_eq!(s.path, "/api/v1/commands/future.thing.do");
    assert!(uuid(&s.body["operation_id"]).is_some());
    assert_eq!(s.body["params"], json!({"x":"1","n":2}));
    fake.run(&["future", "thing", "do", "--param", "x=1"])
        .await
        .unwrap();
    let seen = fake.drain();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].path, "/api/v1/commands?all=true");
    assert_eq!(seen[1].path, "/api/v1/commands/future.thing.do");
    let err = fake.run(&["no", "such", "thing"]).await.unwrap_err();
    assert_eq!(err["code"], "unknown_command");
    assert_eq!(aqv2::exit_code(&err), 1);
}

// ---- typed subcommands: golden requests ----------------------------------

#[path = "channel/cases.rs"]
mod cases;
use cases::REQUEST;
/// Fresh UUIDs (minted by the CLI) become `<new-N>` in order of appearance,
/// so the same minted ID in two places shows as the same placeholder.
fn normalize(
    value: &Value,
    fixed: &BTreeSet<String>,
    minted: &mut BTreeMap<String, String>,
) -> Value {
    match value {
        Value::String(s) if s.parse::<uuid::Uuid>().is_ok() && !fixed.contains(s) => {
            let n = minted.len() + 1;
            json!(
                minted
                    .entry(s.clone())
                    .or_insert_with(|| format!("<new-{n}>"))
            )
        }
        Value::Array(a) => Value::Array(a.iter().map(|v| normalize(v, fixed, minted)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), normalize(v, fixed, minted)))
                .collect::<Map<_, _>>(),
        ),
        other => other.clone(),
    }
}
fn fixed_ids() -> BTreeSet<String> {
    [
        TASK,
        PROJECT,
        ATTEMPT,
        SESSION,
        WORKSPACE,
        OTHER_TASK,
        RECORD,
        COMMENT,
        DECISION,
        WAIT,
        MESSAGE,
        OPERATION,
        CONVERSATION,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}
fn record(mode: aqv2::Mode, args: &[String], seen: Vec<Seen>) -> Value {
    let mode = match mode {
        aqv2::Mode::Owner => "owner",
        aqv2::Mode::Channel => "channel",
    };
    let fixed = fixed_ids();
    let mut minted = BTreeMap::new();
    let calls: Vec<Value> = seen
        .iter()
        .map(|s| json!({"method":s.method,"path":s.path,"body":normalize(&s.body, &fixed, &mut minted)}))
        .collect();
    json!({"mode":mode,"args":args,"calls":calls})
}
#[tokio::test]
async fn typed_subcommands_build_the_golden_requests() {
    let dir = tempfile::tempdir().unwrap();
    let request = dir.path().join("request.json");
    std::fs::write(
        &request,
        br#"{"kind":"inspect","body":{"note":"lifecycle request"}}"#,
    )
    .unwrap();
    let substitute = |args: &[String]| -> Vec<String> {
        args.iter()
            .map(|a| {
                if a == REQUEST {
                    request.to_str().unwrap().to_owned()
                } else {
                    a.clone()
                }
            })
            .collect()
    };
    let mut out = vec![];
    let mut fake = fake(default_reply).await;
    for args in cases::for_mode(aqv2::Mode::Channel) {
        let real = substitute(&args);
        let refs: Vec<&str> = real.iter().map(String::as_str).collect();
        fake.run(&refs)
            .await
            .unwrap_or_else(|e| panic!("{args:?}: {e}"));
        let seen = fake.drain();
        for s in &seen {
            assert_no_credential(s);
        }
        out.push(record(aqv2::Mode::Channel, &args, seen));
    }
    for args in cases::for_mode(aqv2::Mode::Owner) {
        let reply = json!({"api_version":1,"principal":"owner","ok":true});
        let mut srv = server(vec![(StatusCode::OK, reply)]).await;
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        aqv2::execute(srv.client.clone(), &refs)
            .await
            .unwrap_or_else(|e| panic!("{args:?}: {e}"));
        let s = srv.next().await;
        assert!(s.headers.contains_key("x-aq-dev-email"));
        out.push(record(aqv2::Mode::Owner, &args, vec![s]));
    }
    check_golden(
        golden("typed.json"),
        &(serde_json::to_string_pretty(&out).unwrap() + "\n"),
    );
}
/// `--to session` and `--to supervisor` come from `whoami`; a server whose
/// `whoami` lacks them (or has no open conversation) is refused before any
/// mail is sent.
#[tokio::test]
async fn own_session_and_supervisor_mailboxes_need_their_whoami_fields() {
    let mut fake = fake(|method, path, body| match (method, path) {
        ("POST", "/api/v1/commands/whoami") => {
            let mut me = identity();
            me.as_object_mut().unwrap().remove("session_generation");
            me["supervisor_conversation_id"] = Value::Null;
            (StatusCode::OK, me)
        }
        _ => default_reply(method, path, body),
    })
    .await;
    for to in ["session", "supervisor"] {
        let err = fake
            .run(&["message", "send", "x", "--to", to])
            .await
            .unwrap_err();
        assert_eq!(err["code"], "invalid_arguments", "{to}");
        assert!(
            fake.drain()
                .iter()
                .all(|s| s.path == "/api/v1/commands/whoami"),
            "{to}"
        );
    }
}
async fn assert_invalid_channel_arguments(fake: &mut Fake, args: &[&str]) {
    let err = fake.run(args).await.unwrap_err();
    assert_eq!(err["code"], "invalid_arguments", "{args:?}");
    assert_eq!(aqv2::exit_code(&err), 2);
    assert!(
        fake.drain()
            .iter()
            .all(|s| s.path == "/api/v1/commands/whoami"),
        "{args:?}"
    );
}

async fn reject_channel_arguments(fake: &mut Fake) {
    let session_with_id = format!("session:{SESSION}");
    for args in [
        vec!["comment", "add", "x", "--task", OTHER_TASK],
        vec![
            "comment",
            "edit",
            COMMENT,
            "--body",
            "x",
            "--revision",
            "1",
            "--project",
            PROJECT,
        ],
        vec!["task", "create", "--title", "x", "--project", PROJECT],
        vec!["task", "show", "not-a-task"],
        vec!["message", "send", "x", "--to", &session_with_id],
        vec!["message", "send", "x", "--to", "elsewhere"],
        vec!["run", "task.show", "--param", "Bad-Key=1"],
        vec!["run", "task.show", "--param", "a=1", "--param", "a=2"],
        vec!["run", "Task.Show"],
        vec!["conversation", "messages"],
        vec!["conversation", "messages", "--conversation-id", "global"],
        vec![
            "conversation",
            "messages",
            "--conversation-id",
            CONVERSATION,
            "--limit",
            "17",
        ],
        vec![
            "conversation",
            "messages",
            "--conversation-id",
            CONVERSATION,
            "--before",
            "0",
        ],
        vec![
            "conversation",
            "messages",
            "--conversation-id",
            CONVERSATION,
            "--after",
            "1.0",
        ],
        vec![
            "conversation",
            "messages",
            "--conversation-id",
            CONVERSATION,
            "--before",
            "5",
            "--after",
            "2",
        ],
    ] {
        assert_invalid_channel_arguments(fake, &args).await;
    }
}

async fn reject_owner_arguments() {
    let mut srv = server(vec![]).await;
    for args in [
        vec!["task", "show", "self", "--project", PROJECT],
        vec![
            "task",
            "create",
            "--title",
            "x",
            "--parent",
            "self",
            "--project",
            PROJECT,
            "--expected-project-revision",
            "1",
        ],
        vec!["comment", "add", "x"],
        vec!["message", "send", "x"],
        vec!["message", "send", "x", "--to", "supervisor"],
        vec!["message", "inbox", "--to", "session"],
    ] {
        let err = aqv2::execute(srv.client.clone(), &args).await.unwrap_err();
        assert_eq!(err["code"], "invalid_arguments", "{args:?}");
    }
    assert!(srv.seen.try_recv().is_err());
}

#[tokio::test]
async fn typed_subcommands_refuse_mode_mismatches_without_sending() {
    let mut fake = fake(default_reply).await;
    reject_channel_arguments(&mut fake).await;
    reject_owner_arguments().await;
    // Owner subcommands that need the admin listener are not sent through the channel.
    let err = fake.run(&["tasks", "list"]).await.unwrap_err();
    assert_eq!(err["code"], "unsupported");
    assert!(fake.drain().is_empty());
}

// ---- refusals, dispositions, waits ----------------------------------------

#[tokio::test]
async fn a_refusal_names_the_grant_and_exits_14_without_recovery() {
    let mut fake = fake(|method, path, body| match path {
        "/api/v1/commands/task.create" => {
            (StatusCode::FORBIDDEN, fixture("refusal-grant-missing.json"))
        }
        _ => default_reply(method, path, body),
    })
    .await;
    let err = fake
        .run(&["task", "create", "--title", "Unrelated top-level task"])
        .await
        .unwrap_err();
    let sent = fake.drain().pop().unwrap();
    let mut expected = fixture("refusal-grant-missing.json");
    expected["operation_id"] = sent.body["operation_id"].clone();
    assert_eq!(err, expected);
    assert_eq!(aqv2::exit_code(&err), 14);
    assert!(err.get("recovery").is_none());
    // A body that is not a §6.8 refusal is never echoed.
    let mut fake2 = self::fake(|_, _, _| {
        (
            StatusCode::FORBIDDEN,
            json!({"code":"grant_missing","message":"private","effect":"applied","command":"x"}),
        )
    })
    .await;
    let err = fake2.run(&["whoami"]).await.unwrap_err();
    assert_eq!(err["code"], "http_error");
    assert!(!err.to_string().contains("private"));
    fake2.drain();
    for code in aqv2::REFUSED {
        let refusal = json!({"code":code,"command":"task.show","grant":"task.read","held":[],
            "message":"m","effect":"not_applied","request_id":null});
        let f = self::fake(move |_, _, _| (StatusCode::FORBIDDEN, refusal.clone())).await;
        let err = f.run(&["task", "show"]).await.unwrap_err();
        assert_eq!(
            (err["code"].as_str(), aqv2::exit_code(&err)),
            (Some(*code), 14)
        );
    }
}
#[tokio::test]
async fn an_unconfirmed_agent_operation_keeps_its_id_and_is_never_resent() {
    let mut fake = fake(|method, path, body| match path {
        "/api/v1/commands/graph.command" => (
            StatusCode::ACCEPTED,
            json!({"api_version":1,"operation_id":body["operation_id"],"payload_digest":"0".repeat(64),
                "state":"pending","admission":"recorded","effect":"possible"}),
        ),
        _ => default_reply(method, path, body),
    })
    .await;
    let err = fake
        .run(&["knowledge", "author", "--title", "T"])
        .await
        .unwrap_err();
    let seen = fake.drain();
    assert_eq!(seen.len(), 1);
    let id = seen[0].body["operation_id"].as_str().unwrap();
    assert_eq!(err["code"], "operation_unconfirmed");
    assert_eq!(err["operation_id"], id);
    assert_eq!(err["recovery"], format!("aqv2 operation show {id}"));
    assert_eq!(aqv2::exit_code(&err), 10);
    for (state, exit) in [("conflict", 11), ("rejected", 12), ("unknown", 10)] {
        let e = json!({"code":"operation_unconfirmed","operation":{"state":state}});
        assert_eq!(aqv2::exit_code(&e), exit);
    }
}
#[tokio::test]
async fn question_wait_polls_show_until_answered_within_its_bound() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = calls.clone();
    let mut fake = fake(move |_, _, _| {
        let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let state = if n < 1 { "pending" } else { "answered" };
        let mut v = json!({"api_version":1,"decision_id":DECISION,"state":state});
        if state == "answered" {
            v["answer"] = json!("Yes, include it.");
        }
        (StatusCode::OK, v)
    })
    .await;
    let value = fake
        .run(&[
            "question",
            "wait",
            DECISION,
            "--interval",
            "1",
            "--timeout",
            "30",
        ])
        .await
        .unwrap();
    assert_eq!(value["answer"], "Yes, include it.");
    let seen = fake.drain();
    assert_eq!(seen.len(), 2);
    assert!(
        seen.iter()
            .all(|s| s.path == "/api/v1/commands/question.show"
                && s.body == json!({"params":{"decision_id":DECISION}}))
    );
    let mut pending =
        self::fake(|_, _, _| (StatusCode::OK, json!({"api_version":1,"state":"pending"}))).await;
    let started = std::time::Instant::now();
    let err = pending
        .run(&[
            "question",
            "wait",
            DECISION,
            "--interval",
            "1",
            "--timeout",
            "1",
        ])
        .await
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(err["code"], "wait_timeout");
    assert_eq!(aqv2::exit_code(&err), 16);
    assert!(pending.drain().len() >= 2);
}

// ---- human output ----------------------------------------------------------

#[test]
fn human_renderings_match_golden() {
    use aqv2::render::{Render, refusal, render};
    let discovery = json!({"api_version":1,"principal":"agent",
        "commands":[{"name":"task.show","kind":"read","summary":"A task","cli":["task","show"],"grant":"task.read","scope":"project:self"},
                    {"name":"whoami","kind":"read","summary":"Who the caller is","cli":["whoami"],"grant":null,"scope":null}],
        "unavailable":[{"name":"task.cancel","kind":"mutation","summary":"Cancel a task\u{1b}[31m","cli":["task","cancel"],"grant":"task.cancel","reason":"owner_only"}]});
    let mut text = String::new();
    text += &render(Render::Commands, &discovery);
    text += "\n\n";
    text += &render(Render::Whoami, &identity());
    text += "\n\n";
    text += &refusal(&fixture("refusal-grant-missing.json"));
    text += "\n";
    assert!(!text.contains('\u{1b}'));
    check_golden(golden("render.txt"), &text);
}

/// `conversation messages` prints each message's cursor, author kind, time and
/// body, never the conversation's revision (it is not a message count), and
/// tells the reader how to get the next page.
#[test]
fn conversation_messages_print_cursor_author_time_and_body() {
    use aqv2::render::{Render, render};
    let page = json!({"contract":"human-supervisor-conversation-1",
        "conversation":{"id":CONVERSATION,"revision":"41","label":"Global supervisor"},
        "runtime":{"state":"unavailable","binding":null,"reason":"central_runtime_not_connected"},
        "direction":"backward","through_cursor":"0",
        "items":[
            {"cursor":"9","author":{"kind":"human","user_id":OPERATION},"created_at_ms":1_795_368_000_123_i64,
             "body":"Please start\nthe review\u{1b}[31m"},
            {"cursor":"8.1","author":{"kind":"supervisor","actor_id":OPERATION,"generation":2,"session_id":SESSION},
             "created_at_ms":0,"body":"Started"}],
        "next_cursor":"8.1","has_more":true});
    let text = render(Render::Messages, &page);
    assert_eq!(
        text,
        format!(
            "conversation {CONVERSATION} (Global supervisor), 2 messages in this page\n\
             9  human  2026-11-22 17:20:00 UTC\n  Please start\n  the review\\u{{1b}}[31m\n\
             8.1  supervisor  1970-01-01 00:00:00 UTC\n  Started\n\
             more: --before 8.1"
        )
    );
    assert!(!text.contains("41") && !text.to_lowercase().contains("revision"));
    let empty = json!({"conversation":{"id":CONVERSATION,"revision":"41","label":"Global supervisor"},
        "direction":"forward","items":[],"next_cursor":null,"has_more":false});
    assert_eq!(
        render(Render::Messages, &empty),
        format!("conversation {CONVERSATION} (Global supervisor), 0 messages in this page")
    );
}

#[path = "channel/binary.rs"]
mod binary;
#[cfg(feature = "project_environments")]
#[path = "channel/environment_try.rs"]
mod environment_try;

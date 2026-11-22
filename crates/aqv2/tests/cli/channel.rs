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
fn check_golden(name: &str, actual: &str) {
    let path = golden(name);
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

const REQUEST: &str = "@REQUEST";
fn channel_cases() -> Vec<Vec<String>> {
    let c = |args: &[&str]| args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    vec![
        c(&["whoami"]),
        c(&["commands"]),
        c(&["commands", "--all"]),
        c(&["task", "show"]),
        c(&["task", "show", OTHER_TASK]),
        c(&["task", "children"]),
        c(&[
            "task",
            "history",
            "--param",
            &format!("task_id={OTHER_TASK}"),
        ]),
        c(&[
            "task",
            "create",
            "--title",
            "Verify changelog links",
            "--parent",
            "self",
        ]),
        c(&[
            "task",
            "create",
            "--title",
            "Draft one",
            "--body",
            "Body",
            "--acceptance-criteria",
            "Done",
            "--label",
            "docs",
            "--parent",
            OTHER_TASK,
            "--draft",
            "--priority",
            "50",
        ]),
        c(&["task", "create", "--title", "Unrelated top-level task"]),
        c(&[
            "task",
            "graph",
            "show",
            "--param",
            &format!("task_id={OTHER_TASK}"),
        ]),
        c(&["knowledge", "search", "release checklist"]),
        c(&[
            "knowledge",
            "search",
            "release",
            "--kind",
            "knowledge",
            "--limit",
            "5",
            "--cursor",
            "c1",
        ]),
        c(&["knowledge", "show", RECORD]),
        c(&[
            "knowledge",
            "author",
            "--title",
            "Release checklist",
            "--body",
            "Run it.",
            "--label",
            "ops",
            "--content-type",
            "procedure",
        ]),
        c(&["comment", "add", "Found the checklist; following it."]),
        c(&["comment", "list", "--limit", "10"]),
        c(&["comment", "list", "--task", OTHER_TASK]),
        c(&[
            "comment",
            "edit",
            COMMENT,
            "--body",
            "Edited",
            "--revision",
            "2",
        ]),
        c(&["comment", "delete", COMMENT, "--revision", "3"]),
        c(&[
            "question",
            "ask",
            "Should the notes include the deprecated API?",
        ]),
        c(&["question", "show", DECISION]),
        c(&["wait", "register", "--request", REQUEST]),
        c(&["wait", "inspect", "--request", REQUEST]),
        c(&["wait", "ack", "--request", REQUEST]),
        c(&["wait", "list"]),
        c(&["wait", "show", WAIT]),
        c(&["message", "send", "Status: halfway"]),
        c(&[
            "message",
            "send",
            "Need a review",
            "--to",
            &format!("task:{OTHER_TASK}"),
        ]),
        c(&[
            "message",
            "send",
            "Question for you",
            "--to",
            &format!("supervisor:{CONVERSATION}"),
        ]),
        c(&["message", "send", "Hello, supervisor", "--to", "supervisor"]),
        c(&["message", "inbox", "--to", "session"]),
        c(&["message", "inbox"]),
        c(&["message", "body", MESSAGE]),
        c(&["message", "ack", MESSAGE, "--read"]),
        c(&["operation", "show", OPERATION]),
        c(&["operations", "show", OPERATION]),
        c(&[
            "run",
            "task.history",
            "--param",
            &format!("task_id={OTHER_TASK}"),
        ]),
        c(&[
            "run",
            "task.cancel",
            "--subject",
            &format!("task:{OTHER_TASK}:4"),
            "--reason",
            "duplicate",
        ]),
    ]
}
fn owner_cases() -> Vec<Vec<String>> {
    let c = |args: &[&str]| args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    vec![
        c(&["whoami"]),
        c(&["task", "show", OTHER_TASK, "--project", PROJECT]),
        c(&[
            "task",
            "create",
            "--title",
            "Write launch notes",
            "--project",
            PROJECT,
            "--expected-project-revision",
            "3",
            "--parent",
            OTHER_TASK,
            "--label",
            "docs",
        ]),
        c(&["knowledge", "search", "deploy", "--kind", "knowledge"]),
        c(&["knowledge", "show", RECORD, "--project", PROJECT]),
        c(&[
            "knowledge",
            "author",
            "--title",
            "Deploy runbook",
            "--body",
            "Run the migration first.",
            "--label",
            "ops",
            "--content-type",
            "procedure",
            "--project",
            PROJECT,
            "--expected-project-revision",
            "1",
        ]),
        c(&[
            "comment",
            "add",
            "Please also cover phones.",
            "--task",
            OTHER_TASK,
            "--project",
            PROJECT,
        ]),
        c(&[
            "comment",
            "list",
            "--task",
            OTHER_TASK,
            "--project",
            PROJECT,
        ]),
        c(&[
            "comment",
            "edit",
            COMMENT,
            "--body",
            "Edited",
            "--revision",
            "1",
            "--project",
            PROJECT,
        ]),
        c(&[
            "comment",
            "delete",
            COMMENT,
            "--revision",
            "2",
            "--project",
            PROJECT,
        ]),
        c(&["question", "show", DECISION]),
        c(&[
            "message",
            "send",
            "Hello",
            "--to",
            &format!("supervisor:{CONVERSATION}"),
        ]),
        c(&[
            "run",
            "task.cancel",
            "--subject",
            &format!("task:{OTHER_TASK}:4"),
            "--param",
            &format!("project_id={PROJECT}"),
            "--param",
            "expected_version=4",
            "--reason",
            "duplicate",
        ]),
    ]
}
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
fn record(mode: &str, args: &[String], seen: Vec<Seen>) -> Value {
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
    for args in channel_cases() {
        let real = substitute(&args);
        let refs: Vec<&str> = real.iter().map(String::as_str).collect();
        fake.run(&refs)
            .await
            .unwrap_or_else(|e| panic!("{args:?}: {e}"));
        let seen = fake.drain();
        for s in &seen {
            assert_no_credential(s);
        }
        out.push(record("channel", &args, seen));
    }
    for args in owner_cases() {
        let reply = json!({"api_version":1,"principal":"owner","ok":true});
        let mut srv = server(vec![(StatusCode::OK, reply)]).await;
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        aqv2::execute(srv.client.clone(), &refs)
            .await
            .unwrap_or_else(|e| panic!("{args:?}: {e}"));
        let s = srv.next().await;
        assert!(s.headers.contains_key("x-aq-dev-email"));
        out.push(record("owner", &args, vec![s]));
    }
    check_golden(
        "typed.json",
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
#[tokio::test]
async fn typed_subcommands_refuse_mode_mismatches_without_sending() {
    let mut fake = fake(default_reply).await;
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
    ] {
        let err = fake.run(&args).await.unwrap_err();
        assert_eq!(err["code"], "invalid_arguments", "{args:?}");
        assert_eq!(aqv2::exit_code(&err), 2);
        assert!(
            fake.drain()
                .iter()
                .all(|s| s.path == "/api/v1/commands/whoami"),
            "{args:?}"
        );
    }
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
    check_golden("render.txt", &text);
}

// ---- the binary: modes, names, exit codes ----------------------------------

fn bin() -> std::process::Command {
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_aqv2"));
    c.env_remove("AQ_CHANNEL");
    c
}
#[tokio::test(flavor = "multi_thread")]
async fn the_binary_talks_to_the_channel_named_by_aq_channel() {
    let mut fake = fake(|method, path, body| match path {
        "/api/v1/commands/task.create" => {
            (StatusCode::FORBIDDEN, fixture("refusal-grant-missing.json"))
        }
        _ => default_reply(method, path, body),
    })
    .await;
    let channel = format!("unix:{}", fake.socket.display());
    let run = |args: Vec<&'static str>, channel: String| {
        tokio::task::spawn_blocking(move || {
            let mut c = bin();
            c.env("AQ_CHANNEL", channel).args(args).output().unwrap()
        })
    };
    let out = run(vec!["whoami"], channel.clone()).await.unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("principal: agent") && text.contains(&format!("task       {TASK}")),
        "{text}"
    );
    let out = run(vec!["--json", "whoami"], channel.clone())
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v, json!({"schema_version":1,"data":identity()}));

    let out = run(
        vec!["task", "create", "--title", "Unrelated top-level task"],
        channel.clone(),
    )
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(14));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        err.contains("refused (grant_missing): task.create needs task.create.project"),
        "{err}"
    );
    assert!(
        err.contains("Use `aq question ask` to request it."),
        "{err}"
    );
    assert!(
        err.contains("held: task.create.subtask @ task:subtree (max_created_tasks=5, max_depth=2)"),
        "{err}"
    );
    let out = run(
        vec!["task", "create", "--title", "x", "--json"],
        channel.clone(),
    )
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(14));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "grant_missing");
    assert!(out.stderr.is_empty());

    // Channel mode sends no credential, so a token file or origin is refused.
    let out = run(
        vec!["--cf-access-token-file", "/x", "whoami"],
        channel.clone(),
    )
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let out = run(
        vec!["--origin", "https://aq.example", "--json", "whoami"],
        channel.clone(),
    )
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(2));
    // A missing socket is a closed channel: nothing was sent.
    let gone = format!("unix:{}/gone.sock", fake.socket.parent().unwrap().display());
    let out = run(vec!["--json", "whoami"], gone).await.unwrap();
    assert_eq!(out.status.code(), Some(13));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "channel_closed");
    let out = run(vec!["--json", "whoami"], "tcp://127.0.0.1:1".into())
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "configuration_error");
    // `--describe` reads the built-in catalog without a channel.
    let out = run(
        vec!["--json", "run", "task.create", "--describe"],
        "unix:/nonexistent/aq.sock".into(),
    )
    .await
    .unwrap();
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["grant"], "task.create.subtask");
    let seen = fake.drain();
    assert!(
        seen.iter()
            .all(|s| !s.headers.contains_key("authorization"))
    );
}
#[test]
fn installed_as_aq_the_binary_names_itself_aq() {
    let dir = tempfile::tempdir().unwrap();
    let aq = dir.path().join("aq");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_aqv2"), &aq).unwrap();
    let out = std::process::Command::new(&aq)
        .env_remove("AQ_CHANNEL")
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success());
    let help = String::from_utf8(out.stdout).unwrap();
    assert!(help.contains("Usage: aq [OPTIONS] <COMMAND>"), "{help}");
    for group in [
        "run",
        "commands",
        "whoami",
        "task",
        "knowledge",
        "comment",
        "question",
        "wait",
        "message",
    ] {
        assert!(help.contains(&format!("\n  {group} ")), "{group}");
    }
    let out = bin().arg("--help").output().unwrap();
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .contains("Usage: aqv2 [OPTIONS] <COMMAND>")
    );
    // Without a mode the error says how to choose one.
    let out = std::process::Command::new(&aq)
        .env_remove("AQ_CHANNEL")
        .args(["--json", "whoami"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("AQ_CHANNEL")
    );
}

// ---- environment try (spec 0017, feature `project_environments`) ----------

/// The CLI posts `{name, run?}` to the agent route itself (the node's proxy
/// pushes the workspace for that route and adds `commit`), then polls the job
/// and prints its result.
#[cfg(feature = "project_environments")]
#[tokio::test]
async fn environment_try_posts_to_the_agent_route_and_polls_the_job() {
    const JOB: &str = "00000000-0000-4000-8000-0000000000c1";
    let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = polls.clone();
    let result = json!({"job_id":JOB,"outcome":"built","builds":[],"failure":null,"run":{"exit":0,"output":"ok"}});
    let done = result.clone();
    let mut fake = fake(move |method, path, _| match (method, path) {
        ("POST", "/v1/agent/environments/try") => (StatusCode::OK, json!({"job_id":JOB})),
        ("GET", p) if p == format!("/v1/agent/environments/try/{JOB}") => {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let state = if n < 1 { "claimed" } else { "completed" };
            let result = if n < 1 { Value::Null } else { done.clone() };
            (
                StatusCode::OK,
                json!({"job_id":JOB,"state":state,"result":result}),
            )
        }
        _ => (StatusCode::NOT_FOUND, json!({})),
    })
    .await;
    let value = fake
        .run(&[
            "environment",
            "try",
            "web",
            "--run",
            "pnpm test",
            "--interval",
            "1",
        ])
        .await
        .unwrap();
    assert_eq!(value["result"], result);
    let seen = fake.drain();
    assert_eq!(seen.len(), 3);
    assert_eq!(
        (seen[0].method.as_str(), seen[0].path.as_str()),
        ("POST", "/v1/agent/environments/try")
    );
    assert_eq!(
        seen[0].body,
        json!({"api_version":1,"name":"web","run":"pnpm test"})
    );
    assert!(seen[1..].iter().all(|s| s.method == "GET"));
    seen.iter().for_each(assert_no_credential);
    // Without --run the body names only the recipe.
    let mut fake = self::fake(|method, _, _| match method {
        "POST" => (StatusCode::OK, json!({"job_id":JOB})),
        _ => (
            StatusCode::OK,
            json!({"job_id":JOB,"state":"completed","result":{}}),
        ),
    })
    .await;
    fake.run(&["environment", "try", "web"]).await.unwrap();
    assert_eq!(fake.drain()[0].body, json!({"api_version":1,"name":"web"}));
}

#[cfg(feature = "project_environments")]
#[tokio::test]
async fn environment_try_refusals_and_timeouts_are_named() {
    for (status, code) in [
        (StatusCode::CONFLICT, "workspace_not_pushed"),
        (StatusCode::TOO_MANY_REQUESTS, "try_concurrency_reached"),
        (StatusCode::CONFLICT, "no_base_template"),
    ] {
        let body =
            json!({"api_version":1,"code":code,"message":"server text","effect":"not_applied"});
        let mut fake = fake(move |_, _, _| (status, body.clone())).await;
        let err = fake.run(&["environment", "try", "web"]).await.unwrap_err();
        assert_eq!(err["code"], code, "{err}");
        assert!(!err["message"].as_str().unwrap().contains("server text"));
        assert_eq!(aqv2::exit_code(&err), 1);
        assert_eq!(fake.drain().len(), 1, "a refused try is not polled");
    }
    let mut pending = fake(|method, _, _| match method {
        "POST" => (StatusCode::OK, json!({"job_id":OPERATION})),
        _ => (
            StatusCode::OK,
            json!({"job_id":OPERATION,"state":"queued","result":null}),
        ),
    })
    .await;
    let err = pending
        .run(&[
            "environment",
            "try",
            "web",
            "--interval",
            "1",
            "--timeout",
            "1",
        ])
        .await
        .unwrap_err();
    assert_eq!(err["code"], "wait_timeout");
    assert_eq!(aqv2::exit_code(&err), 16);
    assert!(
        err["message"].as_str().unwrap().contains(OPERATION),
        "{err}"
    );
    assert!(pending.drain().len() >= 2);
    // A malformed recipe name is the server's to refuse; flags are the CLI's.
    let err = pending.run(&["environment", "try"]).await.unwrap_err();
    assert_eq!(err["code"], "invalid_arguments");
}

use aq_internal_work::{Affected, AnswerShape, Candidate, Purpose, Request, Way, store, validate};
use aq_session_protocol::Digest;
use aq_supervisor_host::internal::{InternalConfig, recover_run_dirs, serve, tick};
use serde_json::json;
use std::{
    collections::BTreeMap,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

const RUNNER: &str = "internal-host:test";
const ANSWER: &str = r#"{"choice":"yes","rationale":"the input says so"}"#;
/// The fake Claude setup token; its file holds it with a trailing newline.
const CLAUDE_TOKEN: &str = "sk-ant-oat01-fixture-token";

fn login(tag: &str) -> String {
    login_at(tag, "2026-11-12T00:00:00Z")
}

fn login_at(tag: &str, last_refresh: &str) -> String {
    login_for(tag, last_refresh, "account-1")
}

fn login_for(tag: &str, last_refresh: &str, account: &str) -> String {
    json!({"auth_mode":"chatgpt","tokens":{"id_token":format!("id-{tag}"),
            "access_token":format!("access-{tag}"),"refresh_token":format!("refresh-{tag}"),
            "account_id":account},"last_refresh":last_refresh})
    .to_string()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn request() -> Request {
    Request {
        purpose: Purpose::PlaybookCall,
        instructions: "Decide whether the input asks for a yes.".into(),
        input: "Please say yes.".into(),
        answer: AnswerShape::Choice {
            choices: vec!["yes".into(), "no".into()],
            rationale_max_bytes: 200,
        },
        blocking: true,
        affected: Affected {
            kind: "task".into(),
            id: "t-1".into(),
            project_id: None,
        },
        deadline_ms: 60_000,
        max_output_bytes: 4096,
        escalate: false,
        profile_override: None,
        agent: None,
    }
}

fn codex_candidate() -> Candidate {
    Candidate {
        provider: "openai".into(),
        model: "gpt-fixture".into(),
        reasoning: Some("high".into()),
        harness: Some("codex".into()),
        way: Way::Login,
        credential_index: 0,
    }
}

fn claude_candidate() -> Candidate {
    Candidate {
        provider: "anthropic".into(),
        model: "claude-fixture".into(),
        reasoning: Some("default".into()),
        harness: Some("claude".into()),
        ..codex_candidate()
    }
}

/// `candidate` with the effort its level states.
fn with_effort(candidate: Candidate, effort: Option<&str>) -> Candidate {
    Candidate {
        reasoning: effort.map(String::from),
        ..candidate
    }
}

fn api_candidate() -> Candidate {
    Candidate {
        harness: None,
        way: Way::Api,
        ..codex_candidate()
    }
}

/// Replaces `path` atomically with an owner-only file holding `body`.
fn replace(path: &Path, body: &str) {
    let temp = path.with_extension("tmp");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    std::fs::rename(temp, path).unwrap();
}

fn install_engine(root: &Path, rec: &Path) {
    // The engine CLI an agent's shell may run: it records how it ran.
    let engine = root.join("libexec").join("aq-playbooks");
    let recording = format!(
        "#!/bin/sh\nrec='{rec}'\ncat /proc/$$/environ > \"$rec/engine.env\"\n\
                 pwd -P > \"$rec/engine.pwd\"\necho \"$*\" >> \"$rec/engine.args\"\n",
        rec = rec.display()
    );
    std::fs::write(&engine, recording).unwrap();
    std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn install_claude(root: &Path, behaviour: &str, exe_mode: u32) -> serde_json::Value {
    let rec = root.join("rec");
    let dir = root.join("claude-login");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let token = dir.join("token");
    replace(&token, &format!("{CLAUDE_TOKEN}\n"));
    let script = format!(
        "#!/bin/sh\numask 077\nrec='{rec}'\n\
                     for a in \"$@\"; do printf '%s\\0' \"$a\" >> \"$rec/claude.argv\"; done\n\
                     cat /proc/$$/environ > \"$rec/claude.env\"\n\
                     cat > \"$rec/claude.stdin\"\npwd > \"$rec/claude.pwd\"\n\
                     ls -A . > \"$rec/claude.listing\"\nls -A \"$HOME\" > \"$rec/claude.home\"\n\
                     cat \"$HOME/settings.json\" > \"$rec/claude.settings\"\n{behaviour}\n",
        rec = rec.display()
    );
    let native = root.join("claude");
    std::fs::write(&native, &script).unwrap();
    std::fs::set_permissions(&native, std::fs::Permissions::from_mode(exe_mode)).unwrap();
    json!({"native": native, "native_digest": Digest::of(script.as_bytes()),
                    "secret_file": token})
}

struct Fixture {
    _db: aq_server::test_support::TestDb,
    pool: sqlx::PgPool,
    dir: tempfile::TempDir,
    config: std::sync::Arc<InternalConfig>,
}

impl Fixture {
    /// `behaviour` is shell run by the fake native after it has recorded
    /// its argv (NUL separated), stdin, cwd and the cwd's entries; `$out`
    /// is its `--output-last-message` path and `$rec` the record directory.
    async fn new(behaviour: &str) -> Self {
        Self::with_digest(behaviour, None).await
    }

    async fn with_digest(behaviour: &str, digest: Option<Digest>) -> Self {
        Self::build(behaviour, digest, None).await
    }

    /// A host with Claude configured too. `claude` is shell run by the
    /// fake Claude after it has recorded its argv (NUL separated), its
    /// exact environment, stdin, cwd, the cwd's and `$HOME`'s entries and
    /// `$HOME/settings.json` under `$rec/claude.*`.
    async fn with_claude(codex: &str, claude: &str) -> Self {
        Self::build(codex, None, Some(claude)).await
    }

    async fn build(behaviour: &str, digest: Option<Digest>, claude: Option<&str>) -> Self {
        Self::build_as(behaviour, digest, claude, false).await
    }

    /// A host as production runs it, for tests run as root: answer runs
    /// as uid 10001 and agent work as 10002, root-owned files and
    /// parents traversable by both, and a record directory both can
    /// write (each native's own files there stay owner-only).
    async fn as_root(codex: &str, claude: &str) -> Self {
        assert_eq!(unsafe { libc::geteuid() }, 0, "run this test as root");
        Self::build_as(codex, None, Some(claude), true).await
    }

    async fn build_as(
        behaviour: &str,
        digest: Option<Digest>,
        claude: Option<&str>,
        as_root: bool,
    ) -> Self {
        let (dir_mode, exe_mode) = if as_root {
            (0o711, 0o755)
        } else {
            (0o700, 0o700)
        };
        let db = aq_server::test_support::TestDb::create().await;
        drop(db.store().await);
        let pool = db.pool().await;
        let dir = tempfile::Builder::new()
            .prefix("aqih-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(dir_mode)).unwrap();
        for sub in ["homes", "scratch", "rec", "login", "libexec"] {
            let mode = match sub {
                "rec" if as_root => 0o777,
                "homes" | "scratch" | "libexec" => dir_mode,
                _ => 0o700,
            };
            std::fs::create_dir(root.join(sub)).unwrap();
            std::fs::set_permissions(root.join(sub), std::fs::Permissions::from_mode(mode))
                .unwrap();
        }
        let rec = root.join("rec");
        let script = format!(
            "#!/bin/sh\numask 077\nrec='{rec}'\nout=\nwhere=\nprev=\n\
                 if [ \"$1\" = sandbox ]; then\n  [ -e \"$rec/no-sandbox\" ] && exit 1\n\
                 \x20 while [ \"$1\" != -- ]; do shift; done\n  shift\n  exec \"$@\"\nfi\n\
                 for a in \"$@\"; do\n  printf '%s\\0' \"$a\" >> \"$rec/argv\"\n\
                 \x20 [ \"$prev\" = --output-last-message ] && out=$a\n\
                 \x20 [ \"$prev\" = --cd ] && where=$a\n  prev=$a\ndone\n\
                 cat > \"$rec/stdin\"\npwd > \"$rec/pwd\"\nls -A \"$where\" > \"$rec/listing\"\n{behaviour}\n",
            rec = rec.display()
        );
        let native = root.join("codex");
        std::fs::write(&native, &script).unwrap();
        std::fs::set_permissions(&native, std::fs::Permissions::from_mode(exe_mode)).unwrap();
        install_engine(&root, &rec);
        let secret = root.join("login").join("auth.json");
        replace(&secret, &login("A"));
        let digest = digest.unwrap_or_else(|| Digest::of(script.as_bytes()));
        let claude = claude.map(|behaviour| install_claude(&root, behaviour, exe_mode));
        let config: InternalConfig = serde_json::from_value(json!({
            "database_url": "postgres://unused",
            "homes": root.join("homes"),
            "scratch": root.join("scratch"),
            "poll_ms": 100,
            "codex": {"native": native, "native_digest": digest, "secret_file": secret},
            "claude": claude,
        }))
        .unwrap();
        let config = config.with_libexec(root.join("libexec"));
        let config = if as_root { config } else { config.for_tests() };
        Self {
            _db: db,
            pool,
            dir,
            config: std::sync::Arc::new(config),
        }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }
    fn rec(&self, name: &str) -> PathBuf {
        self.root().join("rec").join(name)
    }
    fn secret(&self) -> PathBuf {
        self.root().join("login").join("auth.json")
    }
    fn argv(&self) -> Vec<String> {
        let bytes = std::fs::read(self.rec("argv")).unwrap();
        bytes
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8(a.to_vec()).unwrap())
            .collect()
    }
    fn claude_rec(&self, name: &str) -> String {
        std::fs::read_to_string(self.rec(&format!("claude.{name}"))).unwrap()
    }
    fn claude_argv(&self) -> Vec<String> {
        let argv = self.claude_rec("argv");
        let argv = argv.strip_suffix('\0').unwrap();
        argv.split('\0').map(str::to_owned).collect()
    }
    fn claude_env(&self) -> BTreeMap<String, String> {
        self.claude_rec("env")
            .split('\0')
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                let (key, value) = entry.split_once('=').unwrap();
                (key.to_owned(), value.to_owned())
            })
            .collect()
    }
    async fn submit(&self, candidates: &[Candidate]) -> uuid::Uuid {
        self.submit_request(&request(), candidates).await
    }
    async fn submit_request(&self, request: &Request, candidates: &[Candidate]) -> uuid::Uuid {
        store::submit(&self.pool, request, candidates, now_ms())
            .await
            .unwrap()
    }
    async fn tick(&self) -> bool {
        tick(&self.config, &self.pool, RUNNER, now_ms())
            .await
            .unwrap()
    }
    async fn row(&self, id: uuid::Uuid) -> store::Row {
        store::get(&self.pool, id).await.unwrap().unwrap()
    }
    /// Waits until the fake native has started.
    async fn started(&self) {
        for _ in 0..400 {
            if self.rec("started").exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("fake native never started");
    }
}

fn answering() -> String {
    format!(
        "printf '%s' '{ANSWER}' > \"$out\"\n\
             echo '{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":11,\"cached_input_tokens\":0,\"output_tokens\":7}}}}'"
    )
}

/// Signals start, waits for the test's `go`, rewrites the home login with
/// a refresh of the same account, then answers.
fn refreshing() -> String {
    format!(
        "touch \"$rec/started\"\ni=0\n\
             while [ ! -e \"$rec/go\" ] && [ $i -lt 400 ]; do sleep 0.025; i=$((i+1)); done\n\
             printf '%s' '{}' > \"$CODEX_HOME/auth.json\"\n{}",
        login("B"),
        answering()
    )
}

/// Makes a directory read-only and restores it on drop, so the temp
/// directory can be removed whatever the test's outcome.
struct ReadOnly(PathBuf);
impl ReadOnly {
    fn new(dir: PathBuf) -> Self {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        Self(dir)
    }
}
impl Drop for ReadOnly {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
    }
}

/// A fake Codex whose provider rotates the refresh token on every
/// refresh and rejects a token already used, as the real one does. The
/// provider's current token is in `$rec/current` (the test writes
/// `refresh-A`); a reuse leaves `$rec/reused`. A run on the model
/// `gpt-slow` refreshes and then hangs, taking 0.5 s to exit on SIGTERM.
fn rotating() -> String {
    format!(
        r#"until mkdir "$rec/provider.lock" 2>/dev/null; do sleep 0.01; done
tok=$(sed 's/.*"refresh_token":"\([^"]*\)".*/\1/' "$CODEX_HOME/auth.json")
if [ "$tok" != "$(cat "$rec/current")" ]; then
  rmdir "$rec/provider.lock"
  touch "$rec/reused"
  echo '{{"type":"turn.failed","error":{{"message":"refresh token was already used"}}}}'
  exit 1
fi
printf '%s' "${{tok}}x" > "$rec/current"
sed "s/\"$tok\"/\"${{tok}}x\"/" "$CODEX_HOME/auth.json" > "$CODEX_HOME/next.json"
mv "$CODEX_HOME/next.json" "$CODEX_HOME/auth.json"
rmdir "$rec/provider.lock"
touch "$rec/started"
case " $* " in *" gpt-slow "*) trap 'sleep 0.5; exit 0' TERM; sleep 30 & wait;; esac
sleep 0.5
{}"#,
        answering()
    )
}

/// Runs the host's claim loop until every task in `ids` has ended.
async fn serve_until_done(f: &Fixture, ids: &[uuid::Uuid]) {
    let (config, pool) = (f.config.clone(), f.pool.clone());
    let host = tokio::spawn(serve(config, pool, RUNNER.into(), || Ok(true)));
    let ended = tokio::time::timeout(Duration::from_secs(20), async {
        for id in ids {
            while !matches!(f.row(*id).await.status.as_str(), "answered" | "failed") {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    })
    .await;
    host.abort();
    assert!(ended.is_ok(), "the tasks did not end");
}

/// The refresh token in the login file.
fn refresh_token(f: &Fixture) -> String {
    let login: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(f.secret()).unwrap()).unwrap();
    login["tokens"]["refresh_token"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// A fake Codex that answers, except on the model `gpt-slow`, where it
/// hangs after signalling start.
fn slow_or_answering() -> String {
    format!(
        "touch \"$rec/started\"\ncase \" $* \" in *\" gpt-slow \"*) sleep 30;; esac\n{}",
        answering()
    )
}

fn slow_codex() -> Candidate {
    Candidate {
        model: "gpt-slow".into(),
        ..codex_candidate()
    }
}

/// Waits up to `seconds` for task `id` to be answered.
async fn answered_within(f: &Fixture, id: uuid::Uuid, seconds: u64) -> store::Row {
    let wait = async {
        loop {
            let row = f.row(id).await;
            if row.status == "answered" {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(seconds), wait)
        .await
        .unwrap_or_else(|_| panic!("task {id} was not answered"))
}

/// A 3 s single-candidate request: the runner stops it at 2.7 s.
fn short() -> Request {
    Request {
        deadline_ms: 3_000,
        ..request()
    }
}

/// The object Claude Code prints with `--output-format json`.
fn claude_result(is_error: bool, result: &str) -> String {
    json!({"type":"result","subtype":"success","is_error":is_error,"duration_ms":1840,
            "duration_api_ms":1712,"num_turns":1,"result":result,
            "session_id":"5f0c8d52-0e7a-4c55-9d43-3c1f2b8a7e10","total_cost_usd":0.0123,
            "usage":{"input_tokens":13,"cache_creation_input_tokens":0,
                "cache_read_input_tokens":0,"output_tokens":9}})
    .to_string()
}

/// Shell that prints `stdout`, with `@TOKEN@` replaced by the token the
/// fake Claude found in its environment, and exits with `code`.
fn claude_printing(stdout: &str, code: u8) -> String {
    format!("sed \"s/@TOKEN@/$CLAUDE_CODE_OAUTH_TOKEN/g\" <<'EOF'\n{stdout}\nEOF\nexit {code}")
}

/// The outcome every agent fake saves, as `aq-playbooks save` would.
const OUTCOME: &str = r#"{"kind":"saved","note":"done"}"#;

/// Agent work: a workspace with two files, an outcome checked against a
/// schema, and a host-wide deadline of a minute.
fn agent_request() -> Request {
    Request {
        purpose: Purpose::PlaybookCompilation,
        instructions: "Turn the document into a playbook.".into(),
        input: String::new(),
        answer: AnswerShape::Json {
            schema: json!({"type": "object", "required": ["kind"],
                    "properties": {"kind": {"enum": ["saved", "question", "unable"]}}}),
        },
        blocking: false,
        affected: Affected {
            kind: "playbook_source".into(),
            id: "routing".into(),
            project_id: None,
        },
        agent: Some(aq_internal_work::AgentWork {
            files: [
                ("doc.md".to_string(), "# Routing\nSay yes.".to_string()),
                ("guide/language.md".to_string(), "The language.".to_string()),
            ]
            .into(),
            max_turns: 7,
        }),
        ..request()
    }
}

/// Shell that records the workspace (`$rec/<name>.tree`, the files'
/// paths, and `<name>.doc`) and then saves [`OUTCOME`].
fn saving(name: &str) -> String {
    format!(
        "find . -type f | sort > \"$rec/{name}.tree\"\ncat doc.md > \"$rec/{name}.doc\"\n\
             mkdir -p out\nprintf '%s' '{OUTCOME}' > out/outcome.json"
    )
}

fn left_in(f: &Fixture, dir: &str) -> Vec<std::fs::DirEntry> {
    std::fs::read_dir(f.root().join(dir))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Spawns the host's claim loop.
fn serving(f: &Fixture) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let (config, pool) = (f.config.clone(), f.pool.clone());
    tokio::spawn(serve(config, pool, RUNNER.into(), || Ok(true)))
}

/// Fake Claude: agent work (stream-json output) signals start, counts
/// itself in `$rec/agents` and hangs; an answer task answers.
fn claude_agent_hangs() -> String {
    format!(
        "case \" $* \" in *\" stream-json \"*) echo x >> \"$rec/agents\"; \
             touch \"$rec/started\"; sleep 30;; esac\n{}",
        claude_printing(&claude_result(false, ANSWER), 0)
    )
}

/// The transcript store of one task, by candidate.
async fn transcripts(f: &Fixture, id: uuid::Uuid) -> Vec<(u32, serde_json::Value)> {
    store::transcripts(&f.pool, id).await.unwrap()
}

/// The live processes (not zombies) whose real uid is `uid`.
fn processes_of(uid: u32) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        let field = |key: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(key))
                .map(str::trim)
                .unwrap_or_default()
                .to_owned()
        };
        let real = field("Uid:").split_whitespace().next().map(str::to_owned);
        if real == Some(uid.to_string()) && !field("State:").starts_with('Z') {
            found.push(format!("{name} {}", field("Name:")));
        }
    }
    found
}

/// Waits up to two seconds for no live process of `uid` to be left.
async fn none_left_of(uid: u32) -> Vec<String> {
    for _ in 0..100 {
        let left = processes_of(uid);
        if left.is_empty() {
            return left;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    processes_of(uid)
}

/// Shell that waits up to 10 s for `$rec/<file>`.
fn wait_for(file: &str) -> String {
    format!(
        "i=0; while [ ! -e \"$rec/{file}\" ] && [ $i -lt 400 ]; do sleep 0.025; i=$((i+1)); done"
    )
}

mod agent_outcomes;
mod agent_slots;
mod agent_workspace;
mod budgets;
mod claude_answers;
mod codex_answers;
mod isolation;
mod recovery;
mod scheduling;

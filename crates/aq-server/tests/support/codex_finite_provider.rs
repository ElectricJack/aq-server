//! Finite local HTTP fixture only. All Responses bytes/policy checks come from
//! the pinned worker's pure fixture-responses CLI; no model/provider inference.
#![allow(dead_code)]
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{StatusCode, header},
    response::Response,
    routing::post,
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::watch,
};
const MAX_BODY: usize = 1_048_576;
const MAX_REQUESTS: usize = 7;

/// A pure, already hash-checked executable. This does not admit a runtime.
#[derive(Clone)]
pub struct FixtureCli {
    pub path: PathBuf,
}
impl FixtureCli {
    pub async fn call(&self, input: &Value) -> Result<Value> {
        run_pure(self.path.clone(), vec!["fixture-responses".into()], input).await
    }
    pub async fn export(&self, vectors: &Value) -> Result<Vec<String>> {
        let exported = self.call(vectors).await?;
        ensure!(exported["schema_version"] == 1, "fixture vector version");
        let responses: Vec<String> = serde_json::from_value(exported["responses"].clone())?;
        ensure!(
            !responses.is_empty() && responses.len() <= MAX_REQUESTS,
            "finite fixture response count"
        );
        Ok(responses)
    }
}
async fn run_pure(
    program: PathBuf,
    arguments: Vec<std::ffi::OsString>,
    input: &Value,
) -> Result<Value> {
    let bytes = serde_json::to_vec(input)?;
    ensure!(bytes.len() <= 8 * MAX_BODY, "bounded fixture CLI input");
    let mut child = tokio::process::Command::new(&program)
        .args(arguments)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("pure fixture CLI spawn")?;
    let mut input = child.stdin.take().context("fixture CLI input absent")?;
    let mut output = child.stdout.take().context("fixture CLI output absent")?;
    let operation = async {
        let writer = async move {
            input.write_all(&bytes).await?;
            input.flush().await?;
            // Tokio ChildStdin::shutdown is not EOF. Own and drop the actual
            // handle before awaiting a child that reads its bounded request.
            drop(input);
            Ok::<_, anyhow::Error>(())
        };
        let reader = async {
            let mut bytes = Vec::new();
            (&mut output)
                .take((8 * MAX_BODY + 1) as u64)
                .read_to_end(&mut bytes)
                .await?;
            ensure!(bytes.len() <= 8 * MAX_BODY, "bounded fixture CLI output");
            Ok::<_, anyhow::Error>(bytes)
        };
        let (_, output) = tokio::try_join!(writer, reader)?;
        ensure!(child.wait().await?.success(), "pure fixture CLI refused");
        Ok::<_, anyhow::Error>(serde_json::from_slice(&output)?)
    };
    tokio::time::timeout(Duration::from_secs(2), operation)
        .await
        .context("pure fixture CLI deadline")?
}
#[derive(Clone)]
pub struct ToolProgram {
    pub path: PathBuf,
}
impl ToolProgram {
    async fn call(&self, input: &Value) -> Result<Value> {
        run_pure(
            PathBuf::from("/usr/bin/python3"),
            vec![self.path.as_os_str().to_owned()],
            input,
        )
        .await
    }
}
#[derive(Clone)]
struct Scope {
    binding: Value,
    deadline_ns: u64,
    local_capability: String,
}
#[derive(Clone)]
struct Peer {
    cli: FixtureCli,
    program: ToolProgram,
    model: String,
    upstream_key: String,
    forbidden: Vec<String>,
    completion: String,
    completion_text: String,
    admitted: Arc<AtomicUsize>,
    observed: Arc<Mutex<Vec<Value>>>,
    scope: Arc<Mutex<Option<Scope>>>,
    release: watch::Receiver<bool>,
}
pub struct FinitePeer {
    pub router: Router,
    pub requests: Arc<Mutex<Vec<Value>>>,
    scope: Arc<Mutex<Option<Scope>>>,
    release: watch::Sender<bool>,
}
impl FinitePeer {
    pub async fn new(
        cli: FixtureCli,
        program: ToolProgram,
        model: String,
        upstream_key: String,
        mut forbidden: Vec<String>,
    ) -> Result<Self> {
        ensure!(
            !upstream_key.is_empty(),
            "finite peer synthetic authentication required"
        );
        forbidden.push(upstream_key.clone());
        let vectors = program
            .call(&json!({"operation":"vectors","model":model}))
            .await?;
        let completion_text = vectors["completion"]
            .as_str()
            .context("finite completion data")?
            .to_owned();
        let exported = cli.export(&vectors).await?;
        let completion = exported
            .last()
            .context("finite completion frame absent")?
            .clone();
        let (release, receiver) = watch::channel(false);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let scope = Arc::new(Mutex::new(None));
        let peer = Peer {
            cli,
            program,
            model,
            upstream_key,
            forbidden,
            completion,
            completion_text,
            admitted: Arc::new(AtomicUsize::new(0)),
            observed: requests.clone(),
            scope: scope.clone(),
            release: receiver,
        };
        Ok(Self {
            router: Router::new()
                .route("/v1/responses", post(handle))
                .with_state(peer),
            requests,
            scope,
            release,
        })
    }
    pub async fn tool_validation(&self, program: &ToolProgram, binding: &Value) -> Result<Value> {
        let requests = self.requests.lock().unwrap().clone();
        program
            .call(&json!({"operation":"validate","requests":requests,"binding":binding}))
            .await
    }
    /// Parent borrows the actual readonly worker publication and original
    /// journal deadline after a protected Watch. These are fixture assertions,
    /// never a replacement grant or authority constructed from history.
    pub fn release_after_watch(
        &self,
        binding: Value,
        original_deadline_ns: u64,
        local_capability: String,
    ) -> Result<()> {
        ensure!(
            remaining(original_deadline_ns).is_ok(),
            "original execution already expired"
        );
        ensure!(
            !local_capability.is_empty(),
            "actual local capability required"
        );
        let mut scope = self.scope.lock().unwrap();
        ensure!(scope.is_none(), "finite sequence already released");
        *scope = Some(Scope {
            binding,
            deadline_ns: original_deadline_ns,
            local_capability,
        });
        self.release
            .send(true)
            .context("finite peer no longer alive")?;
        Ok(())
    }
}
fn remaining(deadline_ns: u64) -> Result<Duration> {
    let mut now: nix::libc::timespec = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { nix::libc::clock_gettime(nix::libc::CLOCK_BOOTTIME, &mut now) } == 0,
        "fixture original boot clock"
    );
    let now = u64::try_from(now.tv_sec)?
        .checked_mul(1_000_000_000)
        .and_then(|v| v.checked_add(now.tv_nsec as u64))
        .context("fixture clock overflow")?;
    ensure!(now < deadline_ns, "original execution expired");
    Ok(Duration::from_nanos(deadline_ns - now))
}
async fn handle(State(peer): State<Peer>, request: Request) -> Response {
    match exchange(peer, request).await {
        Ok(body) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(body))
            .unwrap(),
        Err(()) => Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("finite fixture refused"))
            .unwrap(),
    }
}
async fn exchange(mut peer: Peer, request: Request) -> std::result::Result<String, ()> {
    let authorization = format!("Bearer {}", peer.upstream_key);
    if request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        != Some(authorization.as_str())
    {
        return Err(());
    }
    if peer.admitted.fetch_add(1, Ordering::SeqCst) >= MAX_REQUESTS {
        return Err(());
    }
    // No assumption that output exists before a tool call. The parent runs the
    // actual protected Watch and supplies the already-issued original deadline.
    tokio::time::timeout(Duration::from_secs(20), async {
        while !*peer.release.borrow() {
            peer.release.changed().await.map_err(|_| ())?;
        }
        Ok::<_, ()>(())
    })
    .await
    .map_err(|_| ())??;
    let scope = peer.scope.lock().map_err(|_| ())?.clone().ok_or(())?;
    tokio::time::timeout(remaining(scope.deadline_ns).map_err(|_| ())?, async {
        let body = peer.checked_request(request, &scope).await?;
        let requests = peer.record_request(&body)?;
        let next = peer
            .program
            .call(&json!({"operation":"next_tool","requests":requests}))
            .await
            .map_err(|_| ())?;
        let next = &next["next"];
        let response = if next.is_null() {
            let proof = peer
                .program
                .call(&json!({"operation":"validate","requests":requests,"binding":scope.binding}))
                .await
                .map_err(|_| ())?;
            if proof["completion"] != peer.completion_text {
                return Err(());
            }
            peer.completion.clone()
        } else {
            peer.tool_response(next, &body["tools"]).await?
        };
        remaining(scope.deadline_ns).map_err(|_| ())?;
        Ok(response)
    })
    .await
    .map_err(|_| ())?
}

impl Peer {
    async fn checked_request(
        &self,
        request: Request,
        scope: &Scope,
    ) -> std::result::Result<Value, ()> {
        let body = to_bytes(request.into_body(), MAX_BODY - 1)
            .await
            .map_err(|_| ())?;
        let text = std::str::from_utf8(&body).map_err(|_| ())?;
        let mut forbidden = self.forbidden.clone();
        forbidden.push(scope.local_capability.clone());
        let checked = self
            .cli
            .call(
                &json!({"operation":"check","model":self.model,"output_cap":4096,
                "forbidden_material":forbidden,"body_utf8":text}),
            )
            .await
            .map_err(|_| ())?;
        if checked["valid"] != true {
            return Err(());
        }
        serde_json::from_slice(&body).map_err(|_| ())
    }

    fn record_request(&self, body: &Value) -> std::result::Result<Vec<Value>, ()> {
        let mut observed = self.observed.lock().map_err(|_| ())?;
        if observed.len() >= MAX_REQUESTS {
            return Err(());
        }
        observed.push(body.clone());
        Ok(observed.clone())
    }

    async fn tool_response(
        &self,
        next: &Value,
        offered_tools: &Value,
    ) -> std::result::Result<String, ()> {
        match next["tool"].as_str() {
            Some("exec_command") => {
                let value = self
                    .cli
                    .call(&json!({"operation":"vectors","model":self.model,"commands":[{
                        "response_id":next["response_id"],"item_id":next["item_id"],"call_id":next["call_id"],
                        "command":next["arguments"]["cmd"]}],"completion":self.completion_text}))
                    .await
                    .map_err(|_| ())?;
                value["responses"][0].as_str().ok_or(()).map(str::to_owned)
            }
            Some("write_stdin") => {
                let value = self
                    .cli
                    .call(&json!({"operation":"poll","model":self.model,
                        "response_id":next["response_id"],"item_id":next["item_id"],"call_id":next["call_id"],
                        "session_id":next["arguments"]["session_id"],"offered_tools":offered_tools}))
                    .await
                    .map_err(|_| ())?;
                value["response"].as_str().ok_or(()).map(str::to_owned)
            }
            _ => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_peer() -> Peer {
        let (_, release) = watch::channel(false);
        let observed = Arc::new(Mutex::new(Vec::new()));
        Peer {
            cli: FixtureCli {
                path: "/absent".into(),
            },
            program: ToolProgram {
                path: "/absent".into(),
            },
            model: "m".into(),
            upstream_key: "synthetic-upstream".into(),
            forbidden: vec![],
            completion: String::new(),
            completion_text: String::new(),
            admitted: Arc::new(AtomicUsize::new(0)),
            observed,
            scope: Arc::new(Mutex::new(None)),
            release,
        }
    }

    #[test]
    fn transcript_retains_order_and_refuses_overflow_without_mutation() {
        let peer = test_peer();
        for index in 0..MAX_REQUESTS {
            let history = peer.record_request(&json!({"index":index})).unwrap();
            assert_eq!(
                history,
                (0..=index).map(|i| json!({"index":i})).collect::<Vec<_>>()
            );
        }
        let before = peer.observed.lock().unwrap().clone();
        assert!(peer.record_request(&json!({"index":MAX_REQUESTS})).is_err());
        assert_eq!(*peer.observed.lock().unwrap(), before);
    }

    #[tokio::test]
    async fn tool_responses_preserve_command_and_poll_cli_payloads() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("echo-fixture-cli");
        std::fs::write(
            &path,
            "#!/usr/bin/python3\nimport json,sys\nassert sys.argv[1:] == ['fixture-responses']\nvalue=json.dumps(json.load(sys.stdin))\nprint(json.dumps({'responses':[value], 'response':value}))\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut peer = test_peer();
        peer.cli.path = path;
        peer.completion_text = "finite completion".into();
        let offered_tools = json!([{"name":"write_stdin"}]);
        let mut next = json!({
            "tool":"exec_command", "response_id":"response", "item_id":"item", "call_id":"call",
            "arguments":{"cmd":"printf fixture", "session_id":17}
        });
        let response = peer.tool_response(&next, &offered_tools).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap(),
            json!({"operation":"vectors", "model":"m", "commands":[{
                "response_id":"response", "item_id":"item", "call_id":"call", "command":"printf fixture"
            }], "completion":"finite completion"})
        );
        next["tool"] = json!("write_stdin");
        let response = peer.tool_response(&next, &offered_tools).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap(),
            json!({"operation":"poll", "model":"m", "response_id":"response", "item_id":"item",
                "call_id":"call", "session_id":17, "offered_tools":offered_tools})
        );
        next["tool"] = json!("unsupported");
        assert!(peer.tool_response(&next, &offered_tools).await.is_err());
    }

    #[tokio::test]
    async fn pure_json_child_receives_eof_before_parent_waits_for_output() {
        let input = json!({"closed":"public fixture data"});
        let reply=run_pure("/usr/bin/python3".into(),vec!["-c".into(),"import json,sys; value=json.loads(sys.stdin.buffer.read()); print(json.dumps(value))".into()],&input).await.unwrap();
        assert_eq!(reply, input);
    }
    #[tokio::test]
    async fn unauthorized_request_cannot_start_cli_or_record_model_input() {
        let peer = test_peer();
        let observed = peer.observed.clone();
        let response = exchange(
            peer,
            Request::post("/v1/responses")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
        assert!(response.is_err());
        assert!(observed.lock().unwrap().is_empty());
    }
}

//! Test-only actual Graph process/HTTP/SQL owner. No synthetic Graph route or AQ
//! authority is installed here. Explicit shutdown must precede database teardown.
use anyhow::{Context, Result, bail, ensure};
use aq_server::{agent_access::AgentAccessConfig, test_support::TestDb};
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{
    ConnectOptions, Connection, Row,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::SocketAddr,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    str::FromStr,
    time::{Duration, Instant},
};
use uuid::Uuid;
pub const EXISTING_GRAPH_SHA256: &str =
    "eb9c8f41002cb5d9675f87de0cadbe34e7d74cfc60d2449cd23ac4df76921fea";
const MAX_BYTES: usize = 65_536;
#[derive(Clone)]
pub struct BinaryPin {
    pub path: PathBuf,
    pub sha256: String,
}
#[derive(Clone)]
pub struct TaskSeed {
    pub workspace_id: Uuid,
    pub task_id: Uuid,
    pub title: String,
    pub body: String,
    pub acceptance_criteria: String,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct GraphTaskPin {
    pub workspace_id: Uuid,
    pub task_id: Uuid,
    pub version: i64,
    pub content_revision: i64,
}
#[derive(Debug, serde::Serialize)]
pub struct Cleanup {
    pub child_pid: u32,
    pub child_reaped: bool,
    pub successful_exit: bool,
    pub kill_fallback: bool,
    pub database_removed: bool,
    pub directory_removed: bool,
}
pub struct RealGraphFixture {
    child: Option<Child>,
    db: Option<TestDb>,
    directory: Option<tempfile::TempDir>,
    pool: Option<sqlx::PgPool>,
    admin: PgConnectOptions,
    database_name: String,
    origin: String,
    task: GraphTaskPin,
    receipt: Value,
    client: Client,
    token: String,
    token_file: PathBuf,
    actor: String,
    binary_sha256: String,
    stderr: PathBuf,
    stdout: PathBuf,
}
fn private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(path)?;
    file.write_all(bytes)?;
    ensure!(
        file.metadata()?.permissions().mode() & 0o777 == 0o600,
        "private fixture file permissions"
    );
    Ok(())
}
fn bounded_file(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_BYTES,
        "bounded Graph process output exceeded"
    );
    Ok(bytes)
}
fn listening_address(bytes: &[u8]) -> Result<Option<SocketAddr>> {
    ensure!(bytes.len() <= MAX_BYTES, "bounded Graph log");
    // Startup output can be observed between individual writes. Only parse a
    // completed record; partial ANSI/UTF8 remains pending within the same bound.
    if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
        return Ok(None);
    }
    let mut plain = vec![];
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b {
            ensure!(
                bytes.get(index + 1) == Some(&b'['),
                "unsupported Graph log escape"
            );
            index += 2;
            while index < bytes.len() && !(0x40..=0x7e).contains(&bytes[index]) {
                index += 1;
            }
            ensure!(index < bytes.len(), "incomplete Graph log escape");
            index += 1;
        } else {
            plain.push(bytes[index]);
            index += 1;
        }
    }
    let text = std::str::from_utf8(&plain)?;
    let mut found = None;
    for line in text.lines().filter(|l| l.contains("aq-graph listening")) {
        ensure!(found.is_none(), "ambiguous Graph listening record");
        let value = line
            .split_once("address=")
            .context("Graph listening address absent")?
            .1
            .split_whitespace()
            .next()
            .context("Graph address absent")?;
        let addr: SocketAddr = value.parse()?;
        ensure!(
            addr.ip() == std::net::Ipv4Addr::LOCALHOST && addr.port() != 0,
            "Graph fixture must bind numeric IPv4 loopback"
        );
        found = Some(addr);
    }
    Ok(found)
}
fn node_pin(workspace: Uuid, id: Uuid, node: &Value) -> Result<GraphTaskPin> {
    ensure!(
        node["id"] == json!(id)
            && node["workspace_id"] == json!(workspace)
            && node["kind"] == "task"
            && node["deleted"] == false
            && !node["retired"].as_bool().unwrap_or(true),
        "actual Graph task identity/state mismatch"
    );
    let version = node["version"]
        .as_i64()
        .filter(|v| *v > 0)
        .context("actual Graph CAS version absent")?;
    let content_revision = node["content_revision"]
        .as_i64()
        .filter(|v| *v > 0)
        .context("actual Graph logical revision absent")?;
    Ok(GraphTaskPin {
        workspace_id: workspace,
        task_id: id,
        version,
        content_revision,
    })
}
impl RealGraphFixture {
    pub async fn start(pin: BinaryPin, seed: TaskSeed) -> Result<Self> {
        ensure!(
            pin.sha256 == EXISTING_GRAPH_SHA256,
            "only reviewed existing Graph pin is allowed"
        );
        let metadata = fs::symlink_metadata(&pin.path)?;
        ensure!(
            metadata.file_type().is_file()
                && metadata.len() <= 32 * 1024 * 1024
                && metadata.permissions().mode() & 0o111 != 0,
            "Graph pin must be bounded regular executable"
        );
        let image = fs::read(&pin.path)?;
        ensure!(
            hex::encode(Sha256::digest(&image)) == pin.sha256,
            "Graph executable digest mismatch"
        );
        ensure!(
            seed.workspace_id.get_version_num() == 4 && seed.task_id.get_version_num() == 4,
            "fixture workspace/task must be fresh v4 UUIDs"
        );
        ensure!(
            !seed.title.trim().is_empty()
                && seed.title.len() <= 512
                && seed.body.len() <= 8192
                && seed.acceptance_criteria.len() <= 4096,
            "bounded explicit task seed required"
        );
        let admin = PgConnectOptions::from_str(
            &std::env::var("DATABASE_URL").context("test admin database required")?,
        )?;
        ensure!(
            admin.get_host() == "127.0.0.1"
                && admin.get_port() == 55432
                && admin.get_database() == Some("postgres")
                && admin.get_username() == "postgres",
            "fixture requires existing test55432 admin database"
        );
        let directory = tempfile::Builder::new()
            .prefix("aq-gate04-real-graph-")
            .tempdir()?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        let executable = directory.path().join("aq-graph-pinned");
        private_file(&executable, &image)?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o500))?;
        ensure!(
            hex::encode(Sha256::digest(fs::read(&executable)?)) == pin.sha256,
            "private Graph copy mismatch"
        );
        let admin_token = format!(
            "gate04-admin-{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        );
        let token = format!(
            "gate04-service-{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        );
        let admin_token_file = directory.path().join("admin-token");
        let token_file = directory.path().join("service-token");
        private_file(&admin_token_file, admin_token.as_bytes())?;
        private_file(&token_file, token.as_bytes())?;
        let stderr = directory.path().join("graph-stderr");
        let stdout = directory.path().join("graph-stdout");
        private_file(&stderr, b"")?;
        private_file(&stdout, b"")?;
        let db = TestDb::create().await;
        let options = db
            .options()
            .application_name(&format!("gate04-graph-{}", seed.workspace_id));
        let database_name = options
            .get_database()
            .context("owned Graph database absent")?
            .to_owned();
        let suffix = database_name
            .strip_prefix("aq_test_")
            .context("owned Graph database prefix")?;
        ensure!(
            suffix.len() == 32 && suffix.bytes().all(|c| c.is_ascii_hexdigit()),
            "owned database must be exact UUID"
        );
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.clone())
            .await?;
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_millis(500))
            .timeout(Duration::from_secs(2))
            .build()?;
        let actor = format!("gate04-service-{}", Uuid::new_v4().simple());
        let bootstrap_actor = format!("gate04-bootstrap-{}", Uuid::new_v4().simple());
        let mut fixture = Self {
            child: None,
            db: Some(db),
            directory: Some(directory),
            pool: Some(pool),
            admin,
            database_name,
            origin: String::new(),
            task: GraphTaskPin {
                workspace_id: seed.workspace_id,
                task_id: seed.task_id,
                version: 0,
                content_revision: 0,
            },
            receipt: Value::Null,
            client,
            token,
            token_file,
            actor,
            binary_sha256: pin.sha256,
            stderr,
            stdout,
        };
        let dir = fixture.directory.as_ref().unwrap().path();
        fixture.child = Some(
            Command::new(executable)
                .args([
                    "--json",
                    "serve",
                    "--bind",
                    "127.0.0.1:0",
                    "--pool-size",
                    "4",
                    "--bootstrap-workspace",
                    &seed.workspace_id.to_string(),
                    "--bootstrap-actor",
                    &bootstrap_actor,
                    "--bootstrap-token-file",
                ])
                .arg(&admin_token_file)
                .env_clear()
                .env("DATABASE_URL", options.to_url_lossy().as_str())
                .env("HOME", dir)
                .env("PATH", "/usr/bin:/bin")
                .env("LANG", "C.UTF-8")
                .env("RUST_LOG", "aq_graph=info")
                .env("NO_COLOR", "1")
                .current_dir(dir)
                .stdin(Stdio::null())
                .stdout(File::options().append(true).open(&fixture.stdout)?)
                .stderr(File::options().append(true).open(&fixture.stderr)?)
                .spawn()
                .context("spawn exact owned Graph process")?,
        );
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            ensure!(
                fixture.child.as_mut().unwrap().try_wait()?.is_none(),
                "owned Graph process exited during startup"
            );
            if let Some(address) = listening_address(&bounded_file(&fixture.stderr)?)? {
                fixture.origin = format!("http://{address}");
                break;
            }
            ensure!(
                Instant::now() < until,
                "bounded actual Graph startup expired"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (status, health) = fixture
            .request(Method::GET, "/health/ready", None, None)
            .await?;
        ensure!(
            status == StatusCode::OK && health == json!({"status":"ready","api_version":"1"}),
            "actual Graph readiness refused"
        );
        let path = format!("/v1/workspaces/{}/credentials", seed.workspace_id);
        let (status, _) = fixture
            .request(
                Method::POST,
                &path,
                Some(&admin_token),
                Some(json!({"actor_id":fixture.actor,"token":fixture.token,"capability":"write"})),
            )
            .await?;
        ensure!(
            status == StatusCode::OK,
            "actual Graph writer grant refused"
        );
        let mutation = json!({"idempotency_key":Uuid::new_v4().to_string(),"reason":"Owned disposable GATE04 task seed","operations":[{"op":"create_node","id":seed.task_id,"kind":"task","task_state":"active","title":seed.title,"body":seed.body,"acceptance_criteria":seed.acceptance_criteria}]});
        let (status, receipt) = fixture.mutation(mutation.clone()).await?;
        ensure!(
            status == StatusCode::OK
                && receipt["change_set_id"]
                    .as_str()
                    .and_then(|v| Uuid::parse_str(v).ok())
                    .is_some()
                && receipt["results"].as_array().is_some_and(|v| v.len() == 1),
            "actual task creation receipt refused"
        );
        fixture.task = node_pin(seed.workspace_id, seed.task_id, &receipt["results"][0])?;
        ensure!(
            fixture.task.version == 1 && fixture.task.content_revision == 1,
            "fresh task did not begin at actual version1"
        );
        fixture.receipt = receipt;
        let (status, replay) = fixture.mutation(mutation).await?;
        ensure!(
            status == StatusCode::OK && replay == fixture.receipt,
            "actual Graph seed receipt is not stable"
        );
        fixture.assert_task_only_context().await?;
        fixture.owner_evidence().await?;
        Ok(fixture)
    }
    async fn request(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> Result<(StatusCode, Value)> {
        ensure!(
            path.starts_with('/') && !path.starts_with("//") && !path.contains(['#', '\\']),
            "fixed Graph fixture route required"
        );
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.origin))
            .header("X-AQ-Graph-API-Version", "1");
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            ensure!(
                serde_json::to_vec(&body)?.len() <= MAX_BYTES,
                "bounded Graph fixture request"
            );
            request = request.json(&body);
        }
        let mut response = request
            .send()
            .await
            .context("actual Graph HTTP request unavailable")?;
        let status = response.status();
        ensure!(
            response
                .headers()
                .get("X-AQ-Graph-API-Version")
                .is_some_and(|v| v == "1"),
            "Graph API version mismatch"
        );
        ensure!(
            response
                .content_length()
                .is_none_or(|n| n <= MAX_BYTES as u64),
            "bounded Graph fixture response"
        );
        let mut bytes = vec![];
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= MAX_BYTES,
                "bounded Graph fixture response"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok((status, serde_json::from_slice(&bytes)?))
    }
    pub fn origin(&self) -> &str {
        &self.origin
    }
    pub fn task_pin(&self) -> &GraphTaskPin {
        &self.task
    }
    pub fn creation_receipt(&self) -> &Value {
        &self.receipt
    }
    pub fn agent_access_config(&self) -> AgentAccessConfig {
        AgentAccessConfig {
            graph_url: self.origin.clone(),
            graph_token_file: self.token_file.clone(),
            credential_ttl_ms: 10_000,
            mutation_actor_id: Some(self.actor.clone()),
        }
    }
    pub async fn mutation(&self, body: Value) -> Result<(StatusCode, Value)> {
        self.request(
            Method::POST,
            &format!("/v1/workspaces/{}/mutations", self.task.workspace_id),
            Some(&self.token),
            Some(body),
        )
        .await
    }
    pub async fn task(&self) -> Result<Value> {
        let (status, node) = self
            .request(
                Method::GET,
                &format!(
                    "/v1/workspaces/{}/nodes/{}",
                    self.task.workspace_id, self.task.task_id
                ),
                Some(&self.token),
                None,
            )
            .await?;
        ensure!(status == StatusCode::OK, "actual task read refused");
        node_pin(self.task.workspace_id, self.task.task_id, &node)?;
        Ok(node)
    }
    pub async fn assert_task_only_context(&self) -> Result<()> {
        let node = self.task().await?;
        ensure!(
            node["version"] == self.task.version
                && node["content_revision"] == self.task.content_revision
                && node["task_state"] == "active",
            "pilot starting task changed"
        );
        let (status, ready) = self
            .request(
                Method::GET,
                &format!("/v1/workspaces/{}/ready?limit=8", self.task.workspace_id),
                Some(&self.token),
                None,
            )
            .await?;
        ensure!(
            status == StatusCode::OK
                && ready["next_cursor"].is_null()
                && ready["items"]
                    .as_array()
                    .is_some_and(|v| v.len() == 1 && v[0] == node),
            "pilot readiness must contain the actual sole task"
        );
        let (status, context) = self
            .request(
                Method::GET,
                &format!(
                    "/v1/workspaces/{}/nodes/{}/context?depth=1&limit=64&max_bytes=65536",
                    self.task.workspace_id, self.task.task_id
                ),
                Some(&self.token),
                None,
            )
            .await?;
        ensure!(
            status == StatusCode::OK && context["node"] == node && context["truncated"] == false,
            "pilot context must be exact and complete"
        );
        for field in ["nodes", "edges", "pinned_revisions"] {
            ensure!(
                context[field].as_array().is_some_and(|v| v.is_empty()),
                "pilot refuses additional mutable context, never trims it"
            );
        }
        Ok(())
    }
    pub async fn owner_evidence(&self) -> Result<Value> {
        let pool = self
            .pool
            .as_ref()
            .context("owned Graph evidence pool closed")?;
        let rows = sqlx::query("SELECT version,success FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await?;
        ensure!(
            rows.len() == 8
                && rows
                    .iter()
                    .enumerate()
                    .all(|(i, r)| r.get::<i64, _>("version") == i as i64 + 1
                        && r.get::<bool, _>("success")),
            "pinned Graph schema1..8 required"
        );
        let owned:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM credentials WHERE workspace_id=$1 AND actor_id=$2 AND capability='write' AND token_hash=$3)").bind(self.task.workspace_id).bind(&self.actor).bind(hex::encode(Sha256::digest(self.token.as_bytes()))).fetch_one(pool).await?;
        ensure!(owned, "actual writer credential ownership mismatch");
        let counts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM nodes WHERE workspace_id=$1),(SELECT count(*) FROM edges WHERE workspace_id=$1),(SELECT count(*) FROM idempotency_receipts WHERE workspace_id=$1 AND actor_id=$2)").bind(self.task.workspace_id).bind(&self.actor).fetch_one(pool).await?;
        ensure!(
            counts.0 == 1 && counts.1 == 0,
            "pilot Graph must have exactly one task and zero relationships"
        );
        Ok(
            json!({"binary_sha256":self.binary_sha256,"schema":8,"child_pid":self.child.as_ref().context("owned Graph process absent")?.id(),"workspace_id":self.task.workspace_id,"task":self.task,"service_actor":self.actor,"actual_writer_credential_verified":owned,"node_count":counts.0,"edge_count":counts.1,"actual_writer_receipt_count":counts.2,"startup_log_sha256":hex::encode(Sha256::digest(bounded_file(&self.stderr)?))}),
        )
    }
    fn stop_child(&mut self) -> Result<(u32, bool, bool)> {
        let Some(child) = self.child.as_mut() else {
            return Ok((0, true, false));
        };
        let pid = child.id();
        if let Some(status) = child.try_wait()? {
            self.child.take();
            return Ok((pid, status.success(), false));
        }
        // This exact unreaped Child owns pid; no process-name search or external
        // service signal. A zombie cannot have its PID reused until this wait.
        let signaled = unsafe { nix::libc::kill(pid as i32, nix::libc::SIGTERM) };
        ensure!(
            signaled == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(nix::libc::ESRCH),
            "owned Graph SIGTERM failed"
        );
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = child.try_wait()? {
                self.child.take();
                return Ok((pid, status.success(), false));
            }
            if Instant::now() >= until {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        child.kill()?;
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = child.try_wait()? {
                self.child.take();
                return Ok((pid, status.success(), true));
            }
            ensure!(
                Instant::now() < until,
                "owned Graph child exit remains unknown after kill"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn preserve_uncertain(&mut self) {
        if let Some(directory) = &self.directory {
            let _=private_file(&directory.path().join("cleanup-uncertain.json"),
                &serde_json::to_vec(&json!({"database_name":self.database_name,"child_pid":self.child.as_ref().map(Child::id),"child_exit_confirmed":self.child.is_none(),"cleanup":"unconfirmed; only exact owned resources may be inspected"})).unwrap_or_default());
        }
        if let Some(db) = self.db.take() {
            std::mem::forget(db);
        }
        if let Some(directory) = self.directory.take() {
            let path = directory.keep();
            eprintln!(
                "uncertain owned Graph cleanup; private resources retained at {}",
                path.display()
            );
        }
    }
    pub async fn shutdown(mut self) -> Result<Cleanup> {
        let (pid, successful, kill_fallback) = match self.stop_child() {
            Ok(v) => v,
            Err(e) => {
                self.preserve_uncertain();
                return Err(e.context("Graph child exit unknown; owned resources retained"));
            }
        };
        if let Some(pool) = self.pool.take() {
            pool.close().await;
        }
        drop(self.db.take());
        let absent = tokio::time::timeout(Duration::from_secs(3), async {
            let mut admin = sqlx::PgConnection::connect_with(&self.admin).await?;
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname=$1)")
                    .bind(&self.database_name)
                    .fetch_one(&mut admin)
                    .await?;
            admin.close().await?;
            anyhow::Ok(!exists)
        })
        .await;
        if !matches!(absent, Ok(Ok(true))) {
            self.preserve_uncertain();
            bail!("exact owned Graph database cleanup is unconfirmed; private evidence retained");
        }
        if let Some(directory) = self.directory.take() {
            directory.close()?;
        }
        Ok(Cleanup {
            child_pid: pid,
            child_reaped: true,
            successful_exit: successful,
            kill_fallback,
            database_removed: true,
            directory_removed: true,
        })
    }
}
impl Drop for RealGraphFixture {
    fn drop(&mut self) {
        if self.child.is_some() && self.stop_child().is_err() {
            self.preserve_uncertain();
        }
        // Fields drop only after known child exit; the exact TestDb guard removes
        // only its own UUID database. Explicit shutdown also verifies absence.
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn listening_log_is_bounded_exact_loopback_and_rejects_ambiguity() {
        assert_eq!(
            listening_address(b"2026 INFO aq-graph listening address=127.0.0.1:1234\n")
                .unwrap()
                .unwrap()
                .port(),
            1234
        );
        assert_eq!(
            listening_address(b"INFO aq-graph listening \x1b[3maddress\x1b[0m=127.0.0.1:1234\n")
                .unwrap()
                .unwrap()
                .port(),
            1234
        );
        for bytes in [
            b"aq-graph listening address=0.0.0.0:1234\n".as_slice(),
            b"aq-graph listening address=127.0.0.1:0\n",
            b"aq-graph listening address=127.0.0.1:1\naq-graph listening address=127.0.0.1:2\n",
        ] {
            assert!(listening_address(bytes).is_err());
        }
    }
}

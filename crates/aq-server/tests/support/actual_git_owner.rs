//! Shared test-only pinned foreground Git owner; no synthetic AQ Case dependency.
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

pub async fn git(path: &Path, args: &[&str]) -> String {
    let command = std::env::var("AQ_TEST_GIT_EXECUTABLE").expect("explicit pinned Git required");
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(command)
            .current_dir(path)
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.test")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.test")
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
fn pin(name: &str, expected: &str) {
    use sha2::{Digest as _, Sha256};
    use std::io::Read;
    let path = std::env::var(name).expect("explicit executable path required");
    let wanted = std::env::var(expected).expect("explicit executable SHA256 required");
    assert!(Path::new(&path).is_absolute());
    let mut stream = std::fs::File::open(path).unwrap();
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 65536];
    loop {
        let n = stream.read(&mut bytes).unwrap();
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    assert_eq!(
        format!("{:x}", hash.finalize()),
        wanted,
        "qualified executable changed"
    );
}
/// Same exact pin checks used by the qualified two-profile fixture. Call this
/// before any direct Git setup as well as before starting a service owner.
pub fn verify_pins() {
    pin("AQ_TEST_GIT_CANDIDATE", "AQ_TEST_GIT_CANDIDATE_SHA256");
    pin("AQ_TEST_GIT_EXECUTABLE", "AQ_TEST_GIT_EXECUTABLE_SHA256");
}
pub struct Owner {
    pub root: PathBuf,
    pub socket: PathBuf,
    child: std::process::Child,
    leader: Process,
    closed: bool,
    observed: std::collections::BTreeMap<(i32, u64), Process>,
}
impl Owner {
    /// Caller retains private parent state on uncertainty and verifies pins
    /// before seed/setup work. This owns only the foreground Git test process.
    pub async fn start(parent: &Path, label: &str) -> Self {
        let root = parent.join(label);
        let socket = parent.join(format!("{label}.sock"));
        // Default free-space policy is deliberately preserved: no lowering flag.
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new(std::env::var("AQ_TEST_GIT_CANDIDATE").unwrap())
            .args([
                "--state-dir",
                root.to_str().unwrap(),
                "--git-executable",
                &std::env::var("AQ_TEST_GIT_EXECUTABLE").unwrap(),
                "--listen",
                socket.to_str().unwrap(),
                "serve",
            ])
            .process_group(0)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        // std::Child deliberately keeps the leader UNREAPED until every
        // descendant is drained: its kernel PID/PGID cannot be reused meanwhile.
        let leader = match process(child.id() as i32) {
            Ok(Some(leader)) => leader,
            unknown => {
                // No unknown observation becomes proof. The outer private
                // directory was retained before spawn. Only signal the exact
                // unreaped std::Child here; never a guessed descendant group.
                println!(
                    "SOURCE_RETENTION_STARTUP_UNCERTAIN pid={} root={} observation={unknown:?}",
                    child.id(),
                    root.display()
                );
                let _ = child.kill();
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while std::time::Instant::now() < deadline {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                        Err(_) => break,
                    }
                }
                panic!(
                    "startup observation unavailable; private root retained, cleanup NOT proved"
                );
            }
        };
        assert_eq!(leader.group, leader.pid);
        let mut owner = Self {
            root,
            socket,
            child,
            leader: leader.clone(),
            closed: false,
            observed: std::collections::BTreeMap::from([((leader.pid, leader.start), leader)]),
        };
        println!(
            "SOURCE_RETENTION_OWNER group={} start={} session={} root={}",
            owner.leader.pid,
            owner.leader.start,
            owner.leader.session,
            owner.root.display()
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while !owner.socket.exists() {
                let current = process(owner.leader.pid)
                    .expect("foreground observation available")
                    .expect("unreaped leader stays owned");
                assert_eq!(current.start, owner.leader.start);
                assert_ne!(
                    current.state, 'Z',
                    "Git owner refused startup; default policy retained"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        owner.observe_owned().unwrap();
        owner
    }
    pub async fn call(&self, repo: Uuid, operation: &str, params: Value) -> Value {
        let id = Uuid::new_v4();
        let request = json!({"protocol_version":1,"request_id":id.to_string(),"operation_id":id,"operation":operation,"repository_id":repo,"expected_binding_generation":if operation=="repo.register"{0}else{1},"params":params});
        let stream = tokio::net::UnixStream::connect(&self.socket).await.unwrap();
        let mut reader = BufReader::new(stream);
        reader
            .get_mut()
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert!(line.len() < 1024 * 1024);
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["request_id"], id.to_string());
        assert_eq!(reply["ok"], true, "{reply}");
        reply["result"].clone()
    }
    fn observe_owned(&mut self) -> Result<Vec<Process>, String> {
        let rows = processes()?;
        let current = rows
            .iter()
            .find(|p| p.pid == self.leader.pid)
            .ok_or("owned unreaped leader missing")?;
        if current.start != self.leader.start
            || current.group != self.leader.pid
            || current.session != self.leader.session
        {
            return Err("owned leader identity changed".into());
        }
        let mut owned = rows
            .iter()
            .filter(|p| self.observed.contains_key(&(p.pid, p.start)) || p.group == self.leader.pid)
            .map(|p| p.pid)
            .collect::<std::collections::BTreeSet<_>>();
        let mut complete = false;
        for _ in 0..64 {
            let old = owned.len();
            for p in &rows {
                if owned.contains(&p.parent) {
                    owned.insert(p.pid);
                }
            }
            if old == owned.len() {
                complete = true;
                break;
            }
        }
        if !complete {
            return Err("fixture descendant depth bound exceeded".into());
        }
        for p in &rows {
            if owned.contains(&p.pid) {
                // aq-git's legitimate Git children allocate separate PGIDs.
                // Keep their exact identities through reparenting; only their
                // own aq-git supervisor may signal/reap those nested groups.
                // A child that created its own session can remain as an
                // unreaped zombie after termination. It cannot touch
                // private state. Retain its exact identity and require it to
                // disappear before teardown; never signal its foreign group.
                // Any live session escape still fails closed.
                if p.session != self.leader.session && p.state != 'Z' {
                    return Err("Git descendant escaped the owned foreground session".into());
                }
                self.observed.insert((p.pid, p.start), p.clone());
            }
        }
        if self.observed.len() > 16384 {
            return Err("fixture retained identity bound exceeded".into());
        }
        Ok(rows)
    }
    fn close(&mut self) -> Result<(), String> {
        if self.closed {
            return Ok(());
        }
        self.observe_owned()?;
        use nix::{
            sys::signal::{Signal, killpg},
            unistd::Pid,
        };
        let group = Pid::from_raw(self.leader.pid);
        killpg(group, Signal::SIGTERM).map_err(|e| e.to_string())?;
        // Retain the original std::Child unreaped while signaling/draining. A
        // repeated signal is only sent after exact start-time/PGID revalidation.
        for (limit, force) in [
            (std::time::Duration::from_secs(3), false),
            (std::time::Duration::from_secs(2), true),
        ] {
            if force {
                self.observe_owned()?;
                killpg(group, Signal::SIGKILL).map_err(|e| e.to_string())?;
            }
            let deadline = std::time::Instant::now() + limit;
            while std::time::Instant::now() < deadline {
                let rows = self.observe_owned()?;
                let leader = rows
                    .iter()
                    .find(|p| p.pid == self.leader.pid)
                    .ok_or("unreaped leader missing during drain")?;
                if leader.start != self.leader.start
                    || leader.group != self.leader.pid
                    || leader.session != self.leader.session
                {
                    return Err("leader changed during owned drain".into());
                }
                if leader.state == 'Z'
                    && !rows.iter().any(|p| {
                        p.pid != self.leader.pid && self.observed.contains_key(&(p.pid, p.start))
                    })
                {
                    self.child.wait().map_err(|e| e.to_string())?;
                    self.closed = true;
                    println!("SOURCE_RETENTION_OWNER_DRAINED group={}", self.leader.pid);
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        Err("owned Git foreground group failed bounded drain".into())
    }
    pub async fn stop(&mut self) {
        self.close()
            .expect("drain every owned Git descendant before root teardown");
    }
    pub fn artifact(&self, value: &Value) -> PathBuf {
        let id: Uuid = serde_json::from_value(value["artifact_handle"].clone()).unwrap();
        self.root.join("artifacts").join(id.to_string())
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            eprintln!(
                "SOURCE_RETENTION_OWNER_CLEANUP_FAILED group={} root={} error={error}",
                self.leader.pid,
                self.root.display()
            );
        }
    }
}
#[derive(Clone, Debug)]
struct Process {
    pid: i32,
    parent: i32,
    group: i32,
    session: i32,
    start: u64,
    state: char,
}
fn process(pid: i32) -> Result<Option<Process>, String> {
    let bytes = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(bytes) => bytes,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.raw_os_error() == Some(nix::libc::ESRCH) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(format!("process observation unavailable: {error}")),
    };
    let (_, tail) = bytes
        .rsplit_once(") ")
        .ok_or("malformed process observation")?;
    let fields = tail.split_whitespace().collect::<Vec<_>>();
    let number = |index: usize| {
        fields
            .get(index)
            .ok_or("missing process field")?
            .parse::<i32>()
            .map_err(|_| "invalid process field")
    };
    Ok(Some(Process {
        pid,
        parent: number(1)?,
        group: number(2)?,
        session: number(3)?,
        start: fields
            .get(19)
            .ok_or("missing process start")?
            .parse()
            .map_err(|_| "invalid process start")?,
        state: fields
            .first()
            .and_then(|v| v.chars().next())
            .ok_or("missing process state")?,
    }))
}
fn processes() -> Result<Vec<Process>, String> {
    let mut rows = vec![];
    let mut count = 0;
    for entry in std::fs::read_dir("/proc").map_err(|e| e.to_string())? {
        count += 1;
        if count > 16384 {
            return Err("fixture process observation bound exceeded".into());
        }
        let entry = entry.map_err(|e| e.to_string())?;
        if let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>()
            && let Some(row) = process(pid)?
        {
            rows.push(row);
        }
    }
    Ok(rows)
}

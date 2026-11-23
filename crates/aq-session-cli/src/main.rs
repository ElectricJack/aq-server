//! Bounded authenticated relay client. No node credentials, SSH, shell or launch.
use aq_human_http::{Authentication, Client, Limits};
use aq_session_protocol::{InputDisposition, Validate};
use aq_terminal_relay::{
    api::{Mutation, MutationReply, OutputReply, TerminalInfo},
    draft, epoch,
};
use clap::{Parser, Subcommand};
use serde_json::Value;
use std::{
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;
#[derive(Parser)]
#[command(
    name = "aq-session",
    version,
    about = "Inspect exact sessions and attach through authenticated terminal relay"
)]
struct Args {
    #[arg(long)]
    origin: String,
    #[arg(long)]
    cf_access_token_file: Option<PathBuf>,
    #[cfg(feature = "loopback-development")]
    #[arg(long, conflicts_with = "cf_access_token_file")]
    dev_email: Option<String>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Inspect {
        terminal: Uuid,
    },
    /// Watch bounded JSON snapshots/frames; --control explicitly enables literal stdin.
    Attach {
        terminal: Uuid,
        #[arg(long, default_value_t = 30)]
        seconds: u32,
        #[arg(long)]
        control: bool,
    },
    /// Submit one private saved typed intent, including explicit unknown resolution.
    Mutate {
        #[arg(long)]
        request: PathBuf,
    },
    Receipt {
        terminal: Uuid,
        operation: Uuid,
    },
    Logs(Logs),
    Usage {
        attempt: Uuid,
    },
}
#[derive(clap::Args)]
struct Logs {
    attempt: Uuid,
    #[arg(long, default_value_t = 0)]
    after: u64,
    #[arg(long)]
    through: Option<u64>,
    #[arg(long, default_value_t = 16)]
    limit: u32,
}

fn now() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .map_err(|_| "clock unavailable".into())
}
fn print(value: &impl serde::Serialize) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, value).map_err(|_| "output unavailable")?;
    writeln!(out).map_err(|_| "output unavailable".into())
}
fn request(path: PathBuf) -> Result<Mutation, String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "private request unavailable")?;
    let m = file
        .metadata()
        .map_err(|_| "request metadata unavailable")?;
    let regular = m.is_file() && m.nlink() == 1;
    let owner_only = m.uid() == nix::unistd::geteuid().as_raw() && m.mode() & 0o077 == 0;
    if !regular || !owner_only {
        return Err("request must be a private owner-only regular single-link file".into());
    }
    let mut bytes = vec![];
    file.take(65537)
        .read_to_end(&mut bytes)
        .map_err(|_| "request unavailable")?;
    if bytes.len() > 65536 {
        return Err("request byte bound".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "invalid typed request".into())
}
async fn mutate(client: &Client, request: &Mutation) -> Result<MutationReply, String> {
    let (exact, _) = response::expected_identity(request);
    let path = format!("/api/v1/terminals/{}/mutations", exact.terminal_id);
    let reply: MutationReply = client
        .post_json(&path, request)
        .await
        .map_err(|e| e.to_string())?;
    if !response::bound(request, &reply) {
        return Err("mutation reply binding mismatch".into());
    }
    response::check_state(&reply, exact)?;
    Ok(reply)
}
async fn info(client: &Client, id: Uuid) -> Result<TerminalInfo, String> {
    let value: TerminalInfo = client
        .get_json(&format!("/api/v1/terminals/{id}"), &[])
        .await
        .map_err(|e| e.to_string())?;
    value
        .terminal
        .validate()
        .map_err(|_| "invalid terminal response")?;
    if value.terminal.terminal_id != id || value.control.terminal != value.terminal {
        return Err("terminal response identity mismatch".into());
    }
    if value.session_revision == 0 || value.control.session_revision != value.session_revision {
        return Err("terminal response identity mismatch".into());
    }
    validate_policy(&value.terminal_policy)?;
    if let Some(lease) = &value.control.lease {
        lease
            .validate()
            .map_err(|_| "invalid controller response")?;
        if lease.terminal != value.terminal || lease.generation != value.control.generation {
            return Err("controller response identity mismatch".into());
        }
    }
    Ok(value)
}
fn validate_policy(p: &aq_session_protocol::TerminalPolicy) -> Result<(), String> {
    let input = (1..=4096).contains(&p.max_input_bytes)
        && (p.max_input_bytes..=32768).contains(&p.max_pending_input_bytes);
    let output = (1..=65536).contains(&p.max_frame_bytes)
        && (p.max_frame_bytes..=1048576).contains(&p.replay_bytes);
    (input && output)
        .then_some(())
        .ok_or_else(|| "invalid terminal policy response".into())
}

struct Stdin {
    flags: i32,
}
impl Stdin {
    fn new() -> Result<Self, String> {
        // This explicit control client is the sole reader; restore inherited flags on exit.
        let flags = unsafe { nix::libc::fcntl(0, nix::libc::F_GETFL) };
        if flags < 0
            || unsafe { nix::libc::fcntl(0, nix::libc::F_SETFL, flags | nix::libc::O_NONBLOCK) } < 0
        {
            return Err("stdin unavailable".into());
        }
        Ok(Self { flags })
    }
    fn read(&self, max: usize) -> Result<Option<Vec<u8>>, String> {
        let mut bytes = vec![0; max];
        let n = unsafe { nix::libc::read(0, bytes.as_mut_ptr().cast(), bytes.len()) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            return if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::Interrupted
            {
                Ok(None)
            } else {
                Err("stdin unavailable".into())
            };
        }
        bytes.truncate(n as usize);
        Ok(Some(bytes))
    }
}
impl Drop for Stdin {
    fn drop(&mut self) {
        unsafe { nix::libc::fcntl(0, nix::libc::F_SETFL, self.flags) };
    }
}
mod attach;
mod response;
use attach::attach;
async fn run(args: Args) -> Result<(), String> {
    let auth = if let Some(path) = args.cf_access_token_file {
        Authentication::CfAccessTokenFile(path)
    } else {
        #[cfg(feature = "loopback-development")]
        {
            Authentication::LoopbackDevelopmentEmail(
                args.dev_email.ok_or("authentication required")?,
            )
        }
        #[cfg(not(feature = "loopback-development"))]
        {
            return Err("--cf-access-token-file is required".into());
        }
    };
    let client = Client::new(
        &args.origin,
        auth,
        Limits {
            max_request_bytes: 65536,
            max_response_bytes: match &args.command {
                Command::Logs(_) => 262144,
                Command::Usage { .. } => 16384,
                _ => 1048576,
            },
            timeout: Duration::from_secs(4),
        },
    )
    .map_err(|e| e.to_string())?;
    execute(&client, args.command).await
}
async fn execute(client: &Client, command: Command) -> Result<(), String> {
    match command {
        Command::Inspect { terminal } => print(&info(client, terminal).await?),
        Command::Attach {
            terminal,
            seconds,
            control,
        } => attach(client, terminal, seconds, control).await,
        Command::Mutate { request: path } => {
            let request = request(path)?;
            print(&mutate(client, &request).await?)
        }
        Command::Receipt {
            terminal,
            operation,
        } => {
            let r: aq_session_api::InputReceipt = client
                .get_json(
                    &format!("/api/v1/terminals/{terminal}/receipts/{operation}"),
                    &[],
                )
                .await
                .map_err(|e| e.to_string())?;
            if r.terminal.terminal_id != terminal || r.operation_id != operation {
                return Err("receipt scope mismatch".into());
            }
            print(&r)
        }
        Command::Logs(logs) => logs.run(client).await,
        Command::Usage { attempt } => usage(client, attempt).await,
    }
}
impl Logs {
    async fn run(self, client: &Client) -> Result<(), String> {
        let Self {
            attempt,
            after,
            through,
            limit,
        } = self;
        if !(1..=16).contains(&limit) {
            return Err("log page limit must be1..16".into());
        }
        let mut query = vec![
            ("after_sequence", after.to_string()),
            ("limit", limit.to_string()),
        ];
        if let Some(v) = through {
            query.push(("through_sequence", v.to_string()));
        }
        let refs: Vec<_> = query.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let value: Value = client
            .get_json(
                &format!("/api/observations/sessions/{attempt}/events"),
                &refs,
            )
            .await
            .map_err(|e| e.to_string())?;
        let page = aq_observation_api::decode_events(
            &serde_json::to_vec(&value).map_err(|_| "invalid events")?,
        )
        .map_err(|_| "invalid events response")?;
        if (page.binding.attempt_id, page.after_sequence) != (attempt, after)
            || through.is_some_and(|v| v != page.through_sequence)
        {
            return Err("events scope mismatch".into());
        }
        print(&page)
    }
}
async fn usage(client: &Client, attempt: Uuid) -> Result<(), String> {
    let value: Value = client
        .get_json(&format!("/api/observations/sessions/{attempt}/usage"), &[])
        .await
        .map_err(|e| e.to_string())?;
    let usage =
        aq_observation_api::decode_usage(&serde_json::to_vec(&value).map_err(|_| "invalid usage")?)
            .map_err(|_| "invalid usage response")?;
    if usage.binding.attempt_id != attempt {
        return Err("usage scope mismatch".into());
    }
    print(&usage)
}
#[tokio::main]
async fn main() {
    if let Err(error) = run(Args::parse()).await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[test]
    fn saved_literal_input_requires_private_regular_single_link_bounded_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("intent");
        let terminal = aq_session_protocol::TerminalIdentity {
            session: aq_session_protocol::SessionIdentity::Central {
                authority_id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                generation: 1,
            },
            terminal_id: Uuid::new_v4(),
            epoch: 1,
        };
        let intent = Mutation::Input(aq_session_api::InputRequest {
            terminal,
            operation_id: Uuid::new_v4(),
            lease_id: Uuid::new_v4(),
            expected_controller_generation: 1,
            expected_session_revision: 1,
            expires_at_ms: 100,
            bytes: b"private literal input".to_vec(),
            escape_key: false,
        });
        std::fs::write(&path, serde_json::to_vec(&intent).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(request(path.clone()).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(request(path.clone()).is_ok());
        let link = dir.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(request(link).is_err());
        let hard = dir.path().join("hard");
        std::fs::hard_link(&path, &hard).unwrap();
        assert!(request(path.clone()).is_err());
        std::fs::remove_file(hard).unwrap();
        std::fs::write(&path, vec![b' '; 65537]).unwrap();
        assert!(request(path).is_err());
    }
}

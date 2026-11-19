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
    Logs {
        attempt: Uuid,
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long)]
        through: Option<u64>,
        #[arg(long, default_value_t = 16)]
        limit: u32,
    },
    Usage {
        attempt: Uuid,
    },
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
    if !m.is_file()
        || m.nlink() != 1
        || m.uid() != nix::unistd::geteuid().as_raw()
        || m.mode() & 0o077 != 0
    {
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
fn terminal(request: &Mutation) -> &aq_session_protocol::TerminalIdentity {
    match request {
        Mutation::Watch(r) => &r.terminal,
        Mutation::Control(r) => &r.terminal,
        Mutation::Renew(r) => &r.terminal,
        Mutation::Release(r) => &r.terminal,
        Mutation::Input(r) => &r.terminal,
        Mutation::EpochInput(r) => &r.input.terminal,
        Mutation::OpenEpoch(r) => &r.terminal,
        Mutation::Resize(r) => &r.terminal,
        Mutation::Capture(r) => &r.terminal,
        Mutation::Resolve(r) => &r.terminal,
    }
}
async fn mutate(client: &Client, request: &Mutation) -> Result<MutationReply, String> {
    let exact = terminal(request);
    let path = format!("/api/v1/terminals/{}/mutations", exact.terminal_id);
    let reply: MutationReply = client
        .post_json(&path, request)
        .await
        .map_err(|e| e.to_string())?;
    let valid = match (request, &reply) {
        (
            Mutation::Watch(r),
            MutationReply::Watch {
                terminal,
                operation_id,
                subscription_id,
                session_revision,
                expires_at_ms,
            },
        ) => {
            terminal == exact
                && *operation_id == r.operation_id
                && *subscription_id == r.operation_id
                && *session_revision == r.expected_session_revision
                && *expires_at_ms <= r.expires_at_ms
        }
        (
            Mutation::Control(r),
            MutationReply::Control {
                operation_id,
                status,
            },
        ) => {
            *operation_id == r.operation_id
                && status.terminal == *exact
                && status.session_revision == r.expected_session_revision
        }
        (
            Mutation::Renew(r),
            MutationReply::Control {
                operation_id,
                status,
            },
        ) => {
            *operation_id == r.operation_id
                && status.terminal == *exact
                && status.session_revision == r.expected_session_revision
        }
        (
            Mutation::Release(r),
            MutationReply::Released {
                terminal,
                operation_id,
                generation,
            },
        ) => {
            terminal == exact
                && *operation_id == r.operation_id
                && Some(*generation) == r.expected_controller_generation.checked_add(1)
        }
        (Mutation::Input(r), MutationReply::Input(receipt)) => {
            receipt.terminal == *exact && receipt.operation_id == r.operation_id
        }
        (Mutation::EpochInput(r), MutationReply::Input(receipt)) => {
            receipt.terminal == *exact && receipt.operation_id == r.input.operation_id
        }
        (Mutation::Resolve(r), MutationReply::Input(receipt)) => {
            receipt.terminal == *exact && receipt.operation_id == r.uncertain_operation_id
        }
        (
            Mutation::Resize(r),
            MutationReply::Resize {
                terminal,
                operation_id,
                sequence,
            },
        ) => terminal == exact && *operation_id == r.operation_id && *sequence == r.sequence,
        (
            Mutation::Capture(r),
            MutationReply::Capture {
                terminal,
                operation_id,
                capture_request_id,
                expires_at_ms,
            },
        ) => {
            terminal == exact
                && *operation_id == r.operation_id
                && !capture_request_id.is_nil()
                && *expires_at_ms <= r.expires_at_ms
        }
        (
            Mutation::OpenEpoch(r),
            MutationReply::Epoch {
                operation_id,
                permit,
            },
        ) => {
            *operation_id == r.operation_id
                && permit.validate().is_ok()
                && permit.terminal == *exact
                && permit.lease_id == r.lease_id
                && permit.controller_generation == r.expected_controller_generation
        }
        _ => false,
    };
    if !valid {
        return Err("mutation reply binding mismatch".into());
    }
    if let MutationReply::Control { status, .. } = &reply
        && let Some(lease) = &status.lease
    {
        lease.validate().map_err(|_| "invalid controller reply")?;
        if lease.terminal != *exact || lease.generation != status.generation {
            return Err("controller reply binding mismatch".into());
        }
    }
    if let MutationReply::Input(receipt) = &reply
        && receipt.disposition == InputDisposition::DeliveryUnknown
        && !receipt.input_paused
    {
        return Err("invalid unknown receipt".into());
    }
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
    if value.terminal.terminal_id != id
        || value.control.terminal != value.terminal
        || value.session_revision == 0
        || value.control.session_revision != value.session_revision
    {
        return Err("terminal response identity mismatch".into());
    }
    let p = &value.terminal_policy;
    if p.max_input_bytes == 0
        || p.max_input_bytes > 4096
        || p.max_pending_input_bytes < p.max_input_bytes
        || p.max_pending_input_bytes > 32768
        || p.max_frame_bytes == 0
        || p.max_frame_bytes > 65536
        || p.replay_bytes < p.max_frame_bytes
        || p.replay_bytes > 1048576
    {
        return Err("invalid terminal policy response".into());
    }
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
async fn attach(client: &Client, id: Uuid, seconds: u32, control: bool) -> Result<(), String> {
    if !(1..=300).contains(&seconds) {
        return Err("attach duration must be1..300 seconds".into());
    }
    let initial = info(client, id).await?;
    let exact = initial.terminal.clone();
    let end = tokio::time::Instant::now() + Duration::from_secs(seconds as u64);
    let mut sub = Uuid::nil();
    let mut watch_until = 0;
    let mut after = 0;
    let mut stdin = if control { Some(Stdin::new()?) } else { None };
    let mut lease: Option<aq_session_protocol::ControllerLease> = None;
    let mut permit: Option<epoch::Permit> = None;
    let mut sequence = 0;
    let mut pending: Option<Uuid> = None;
    if control {
        if !initial.terminal_policy.input_enabled {
            return Err("terminal is output-only".into());
        }
        let reply = mutate(
            client,
            &Mutation::Control(aq_session_api::AcquireControlRequest {
                terminal: exact.clone(),
                operation_id: Uuid::new_v4(),
                expected_session_revision: initial.session_revision,
                expected_controller_generation: initial.control.generation,
                expires_at_ms: now()?.saturating_add(15000),
            }),
        )
        .await?;
        let MutationReply::Control { status, .. } = reply else {
            return Err("unexpected controller reply".into());
        };
        lease = status.lease.clone();
        print(&status)?;
    }
    while tokio::time::Instant::now() < end {
        let clock = now()?;
        let current = info(client, id).await?;
        if current.terminal != exact || current.session_revision != initial.session_revision {
            return Err("session authority changed; attach again explicitly".into());
        }
        if clock.saturating_add(5000) >= watch_until {
            sub = Uuid::new_v4();
            watch_until = clock.saturating_add(20000);
            print(
                &mutate(
                    client,
                    &Mutation::Watch(aq_session_api::WatchRequest {
                        terminal: exact.clone(),
                        operation_id: sub,
                        expected_session_revision: current.session_revision,
                        expires_at_ms: watch_until,
                        after_sequence: Some(after),
                    }),
                )
                .await?,
            )?;
        }
        let after_text = after.to_string();
        let sub_text = sub.to_string();
        let output: OutputReply = client
            .get_json(
                &format!("/api/v1/terminals/{id}/output"),
                &[
                    ("subscription_id", &sub_text),
                    ("after_sequence", &after_text),
                ],
            )
            .await
            .map_err(|e| e.to_string())?;
        match &output {
            OutputReply::Ready {
                view,
                snapshot,
                frames,
                next_sequence,
                ..
            } => {
                snapshot.validate().map_err(|_| "invalid snapshot")?;
                if snapshot.state.len() > current.terminal_policy.replay_bytes as usize {
                    return Err("snapshot exceeds sealed policy".into());
                }
                if view.terminal != exact
                    || snapshot.terminal != exact
                    || view.snapshot_id != snapshot.snapshot_id
                    || view.through_sequence != snapshot.through_sequence
                    || view.subscription_id != sub
                    || *next_sequence < after
                {
                    return Err("output response identity/barrier mismatch".into());
                }
                if view.session_revision != initial.session_revision
                    || view.columns != snapshot.columns
                    || view.rows != snapshot.rows
                    || view.snapshot_reference.identity != snapshot.snapshot_id.to_string()
                    || view.snapshot_reference.digest != snapshot.digest.as_str()
                    || view.renderer_reference.identity != snapshot.renderer.id.as_str()
                    || view.renderer_reference.digest != snapshot.renderer.digest.as_str()
                    || view.renderer_reference.revision != snapshot.renderer.version.as_str()
                {
                    return Err("output reference binding mismatch".into());
                }
                let encoded =
                    serde_json::to_vec(&aq_session_api::ContractDocument::WatchView(view.clone()))
                        .map_err(|_| "invalid watch view")?;
                aq_session_api::decode(&encoded).map_err(|_| "invalid watch view")?;
                let mut previous = after.max(snapshot.through_sequence);
                for frame in frames {
                    frame.validate().map_err(|_| "invalid frame")?;
                    if frame.bytes.len() > current.terminal_policy.max_frame_bytes as usize {
                        return Err("frame exceeds sealed policy".into());
                    }
                    if frame.terminal != exact || frame.sequence <= previous {
                        return Err("output frame barrier/order mismatch".into());
                    }
                    previous = frame.sequence;
                }
                if *next_sequence != previous {
                    return Err("output continuation mismatch".into());
                }
                after = *next_sequence;
            }
            OutputReply::Pending {
                terminal,
                subscription_id,
                ..
            } => {
                if *terminal != exact || *subscription_id != sub {
                    return Err("pending response identity mismatch".into());
                }
            }
        }
        print(&output)?; // JSON escapes control bytes; no unqualified renderer executes ANSI.
        if let Some(operation) = pending {
            let receipt: aq_session_api::InputReceipt = client
                .get_json(&format!("/api/v1/terminals/{id}/receipts/{operation}"), &[])
                .await
                .map_err(|e| e.to_string())?;
            if receipt.operation_id != operation || receipt.terminal != exact {
                return Err("input receipt identity mismatch".into());
            }
            print(&receipt)?;
            match receipt.disposition {
                InputDisposition::DeliveryUnknown => {
                    stdin = None;
                    pending = None;
                    eprintln!(
                        "input {operation} is unknown; explicit fresh capture and resolution required"
                    );
                }
                InputDisposition::Applied
                | InputDisposition::Expired
                | InputDisposition::Rejected => pending = None,
                _ => {}
            }
        }
        let clock = now()?;
        if let Some(old) = lease.clone() {
            if current
                .control
                .lease
                .as_ref()
                .is_none_or(|l| l.lease_id != old.lease_id)
            {
                return Err("controller replaced; input stopped".into());
            }
            if current.control.input_paused {
                stdin = None;
            }
            if pending.is_none()
                && clock
                    >= old.issued_at_ms.saturating_add(
                        old.expires_at_ms
                            .saturating_sub(old.issued_at_ms)
                            .saturating_mul(2)
                            / 3,
                    )
                && stdin.is_some()
            {
                let reply = mutate(
                    client,
                    &Mutation::Renew(draft::RenewControl {
                        terminal: exact.clone(),
                        operation_id: Uuid::new_v4(),
                        expected_session_revision: current.session_revision,
                        expected_controller_generation: old.generation,
                        lease_id: old.lease_id,
                        expires_at_ms: clock.saturating_add(15000),
                    }),
                )
                .await?;
                let MutationReply::Control { status, .. } = reply else {
                    return Err("unexpected renewal reply".into());
                };
                lease = status.lease.clone();
                permit = None;
                sequence = 0;
                print(&status)?;
            } else if current.control.fence_acknowledged
                && pending.is_none()
                && clock.saturating_add(250) < old.expires_at_ms
                && permit
                    .as_ref()
                    .is_none_or(|p| clock.saturating_add(250) < p.expires_at_ms)
            {
                if current.input_mode == draft::InputMode::EpochV1 && permit.is_none() {
                    let reply = mutate(
                        client,
                        &Mutation::OpenEpoch(epoch::Open {
                            terminal: exact.clone(),
                            operation_id: Uuid::new_v4(),
                            expected_session_revision: current.session_revision,
                            expected_controller_generation: old.generation,
                            lease_id: old.lease_id,
                            expires_at_ms: now()?.saturating_add(2000),
                        }),
                    )
                    .await?;
                    let MutationReply::Epoch { permit: p, .. } = reply else {
                        return Err("unexpected epoch reply".into());
                    };
                    p.validate().map_err(|_| "invalid input epoch")?;
                    if p.terminal != exact
                        || p.lease_id != old.lease_id
                        || p.controller_generation != old.generation
                        || p.expires_at_ms > old.expires_at_ms
                    {
                        return Err("input epoch binding mismatch".into());
                    }
                    permit = Some(p);
                }
                if let Some(reader) = &stdin
                    && let Some(bytes) =
                        reader.read(current.terminal_policy.max_input_bytes as usize)?
                {
                    if bytes.is_empty() {
                        stdin = None;
                    } else {
                        sequence += 1;
                        let operation = if let Some(p) = &permit {
                            epoch::operation_id(p, sequence)
                                .map_err(|_| "input ID derivation failed")?
                        } else {
                            Uuid::new_v4()
                        };
                        let input = aq_session_api::InputRequest {
                            terminal: exact.clone(),
                            operation_id: operation,
                            lease_id: old.lease_id,
                            expected_controller_generation: old.generation,
                            expected_session_revision: current.session_revision,
                            expires_at_ms: now()?
                                .saturating_add(2000)
                                .min(old.expires_at_ms)
                                .min(permit.as_ref().map_or(u64::MAX, |p| p.expires_at_ms)),
                            bytes,
                            escape_key: false,
                        };
                        let request = if let Some(p) = &permit {
                            Mutation::EpochInput(epoch::Request {
                                version: 1,
                                capability: aq_session_protocol::Text::new(epoch::CAPABILITY)
                                    .map_err(|_| "invalid epoch capability")?,
                                epoch_id: p.epoch_id,
                                sequence,
                                input,
                            })
                        } else {
                            Mutation::Input(input)
                        };
                        let reply=mutate(client,&request).await.map_err(|_|format!("input {operation} outcome unknown; inspect its receipt; do not resend"))?;
                        print(&reply)?;
                        pending = Some(operation);
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if pending.is_some() {
        eprintln!("input receipt remains pending; inspect its operation ID before further input");
    }
    if let Some(old) = lease {
        let _ = mutate(
            client,
            &Mutation::Release(draft::ReleaseControl {
                terminal: exact,
                operation_id: Uuid::new_v4(),
                expected_session_revision: initial.session_revision,
                expected_controller_generation: old.generation,
                lease_id: old.lease_id,
            }),
        )
        .await;
    }
    Ok(())
}
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
                Command::Logs { .. } => 262144,
                Command::Usage { .. } => 16384,
                _ => 1048576,
            },
            timeout: Duration::from_secs(4),
        },
    )
    .map_err(|e| e.to_string())?;
    match args.command {
        Command::Inspect { terminal } => print(&info(&client, terminal).await?),
        Command::Attach {
            terminal,
            seconds,
            control,
        } => attach(&client, terminal, seconds, control).await,
        Command::Mutate { request: path } => {
            let request = request(path)?;
            print(&mutate(&client, &request).await?)
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
        Command::Logs {
            attempt,
            after,
            through,
            limit,
        } => {
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
            if page.binding.attempt_id != attempt
                || page.after_sequence != after
                || through.is_some_and(|v| v != page.through_sequence)
            {
                return Err("events scope mismatch".into());
            }
            print(&page)
        }
        Command::Usage { attempt } => {
            let value: Value = client
                .get_json(&format!("/api/observations/sessions/{attempt}/usage"), &[])
                .await
                .map_err(|e| e.to_string())?;
            let usage = aq_observation_api::decode_usage(
                &serde_json::to_vec(&value).map_err(|_| "invalid usage")?,
            )
            .map_err(|_| "invalid usage response")?;
            if usage.binding.attempt_id != attempt {
                return Err("usage scope mismatch".into());
            }
            print(&usage)
        }
    }
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

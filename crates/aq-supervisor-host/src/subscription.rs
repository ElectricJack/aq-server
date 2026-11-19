//! Subscription-backed native turn in its own central container/UID boundary.
use crate::config::{Config, protected_read};
use anyhow::{Context, Result, ensure};
use aq_session_protocol::Digest;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt},
        process::CommandExt,
    },
    path::Path,
};
use zeroize::Zeroizing;

/// The uid and gid agent work runs as; ordinary runs use [`Identity::NATIVE`]'s.
pub(crate) const AGENT_UID: u32 = 10002;

/// Who must own the native executable, and who the native child runs as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    /// Owner uid the verified native executable must have.
    pub(crate) executable_owner: u32,
    /// uid and gid of the native child, and owner of the files it writes.
    pub(crate) run_as: u32,
    /// Drop to `run_as` before exec and chown the child's files to it.
    pub(crate) drop_privileges: bool,
}

impl Identity {
    /// The only identity production builds: a root-owned executable run as 10001.
    pub(crate) const NATIVE: Identity = Identity {
        executable_owner: 0,
        run_as: 10001,
        drop_privileges: true,
    };

    /// The identity agent work runs as (spec 0013 §4.2): its own uid, which
    /// no other run uses, so an agent cannot reach another run's process,
    /// home or workspace, and every process of that uid can be killed when
    /// the agent run ends. Without a privilege drop (tests) it is unchanged.
    pub(crate) fn agent(self) -> Identity {
        match self.drop_privileges {
            true => Identity {
                run_as: AGENT_UID,
                ..self
            },
            false => self,
        }
    }

    /// The current uid, with no privilege drop and no chown, for tests that
    /// do not run as root.
    #[cfg(feature = "test-support")]
    pub(crate) fn current() -> Identity {
        let uid = unsafe { libc::geteuid() };
        Identity {
            executable_owner: uid,
            run_as: uid,
            drop_privileges: false,
        }
    }
}

impl Default for Identity {
    fn default() -> Self {
        Self::NATIVE
    }
}

pub async fn native(
    c: &Config,
    directory: &Path,
    home: &Path,
    credential: &Path,
    startup: &[u8],
) -> Result<()> {
    let identity = Identity::NATIVE;
    unsafe {
        libc::umask(0o077);
    }
    let settings = c
        .subscription
        .as_ref()
        .context("subscription mode missing")?;
    crate::tool::origin(&settings.server_origin)?;
    ensure!(
        settings.instructions.len() <= 131072,
        "supervisor instruction bound"
    );
    let auth = stage_login(&c.secret_file, home, identity)?;
    let auth_path = home.join("auth.json");
    let executable = verify_native(&c.native, &c.native_digest, identity)?;
    let output = home.join("last-message.txt");
    let actions = home.join("action-receipts");
    let args = exec_arguments(
        "danger-full-access",
        c.model.model.as_str(),
        &c.workspace,
        &output,
    )?;
    let mut overrides = exec_overrides(Some(c.model.reasoning.as_str()));
    overrides.push(format!(
        "developer_instructions={}",
        quote(&settings.instructions)
    ));
    for (key, value) in [
        (
            "AQ_SUPERVISOR_CREDENTIAL_FILE",
            credential.to_string_lossy().into_owned(),
        ),
        ("AQ_SUPERVISOR_ORIGIN", settings.server_origin.clone()),
        (
            "AQ_SUPERVISOR_ACTIONS_FILE",
            actions.to_string_lossy().into_owned(),
        ),
    ] {
        overrides.push(format!(
            "shell_environment_policy.set.{key}={}",
            quote(&value)
        ));
    }
    overrides.extend(service_tier(&settings.options)?);
    let mut command = native_command(
        &executable,
        exec_finish(args, overrides),
        home,
        &c.workspace,
        identity,
    );
    let mut child = command.spawn()?;
    child
        .stdin
        .take()
        .context("native stdin")?
        .write_all(startup)?;
    let status = crate::terminal::wait(c, child).await?;
    // Retain only a validated refresh of this subscription account inside this
    // central container; never write the operator's original host auth document.
    let copied = copy_back(&c.secret_file, &auth_path, &auth, identity);
    let _ = std::fs::remove_file(&auth_path);
    copied?;
    ensure!(status.success(), "native supervisor turn failed");
    let mut text = read_message(&output)?;
    let aq_token = Zeroizing::new(protected_read_native(credential, identity.run_as)?);
    text = text.replace(std::str::from_utf8(&aq_token)?, "[credential redacted]");
    let text = redact_login(
        text,
        &[
            &auth,
            &Zeroizing::new(protected_read(&c.secret_file, 65536)?),
        ],
    )?;

    publish(c, directory, credential, &text, &actions).await
}

/// JSON string literal for a `--config` value.
pub(crate) fn quote(v: &str) -> String {
    serde_json::to_string(v).expect("text serializes")
}

/// Exclusive `flock` on a read-only descriptor of the login file's
/// directory, released when the descriptor drops. Every copy of a login file
/// in or back holds it, so no two runs on this host read or replace the file
/// at the same time. It needs no write access and creates nothing, and the
/// directory stays the same inode across the rename that replaces the file.
pub(crate) fn lock_login(secret_file: &Path) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let directory = secret_file.parent().context("login file directory")?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(directory)?;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(file);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
}

/// Copies the validated ChatGPT login at `secret_file` to `home/auth.json`
/// for the native child, under the login lock. Returns the bytes copied in.
pub(crate) fn stage_login(
    secret_file: &Path,
    home: &Path,
    identity: Identity,
) -> Result<Zeroizing<Vec<u8>>> {
    let _lock = lock_login(secret_file)?;
    let auth = Zeroizing::new(protected_read(secret_file, 65536)?);
    aq_session_host::PrivateLaunchInput::validate_chatgpt_auth_json(&auth)?;
    let auth_path = home.join("auth.json");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&auth_path)?
        .write_all(&auth)?;
    if identity.drop_privileges {
        let id = Some(identity.run_as);
        std::os::unix::fs::chown(&auth_path, id, id)?;
        std::os::unix::fs::chown(home, id, id)?;
    }
    Ok(auth)
}

/// Copies the child's refreshed `auth.json` back to `secret_file` when it is
/// a valid, changed login of the same account and the secret file still
/// holds exactly the bytes `original` copied in (compare-and-swap under the
/// login lock). A file someone else refreshed meanwhile is left as it is.
/// Returns the refreshed login, if the child left a valid one.
pub(crate) fn copy_back(
    secret_file: &Path,
    auth_path: &Path,
    original: &[u8],
    identity: Identity,
) -> Result<Option<Zeroizing<Vec<u8>>>> {
    let Ok(refreshed) = protected_read_native(auth_path, identity.run_as) else {
        return Ok(None);
    };
    if aq_session_host::PrivateLaunchInput::validate_chatgpt_auth_json(&refreshed).is_err() {
        return Ok(None);
    }
    let before: Value = serde_json::from_slice(original)?;
    let after: Value = serde_json::from_slice(&refreshed)?;
    if before["tokens"]["account_id"] == after["tokens"]["account_id"]
        && refreshed.as_slice() != original
    {
        let _lock = lock_login(secret_file)?;
        let current = Zeroizing::new(protected_read(secret_file, 65536)?);
        if Digest::of(&current) != Digest::of(original) {
            eprintln!("login file changed during the native run; kept it and dropped this refresh");
        } else {
            replace_login(secret_file, &refreshed)?;
        }
    }
    Ok(Some(refreshed))
}

/// Replaces `secret_file` with `login`: an owner-only temporary file renamed
/// over it. The caller holds the login lock.
fn replace_login(secret_file: &Path, login: &[u8]) -> Result<()> {
    let temp = secret_file.with_extension(format!("refresh-{}", uuid::Uuid::new_v4()));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?
        .write_all(login)?;
    std::fs::rename(temp, secret_file)?;
    Ok(())
}

/// Offers `leftover`, the login a Codex run that never finished left in its
/// home, to `secret_file` under the login lock. It is copied in when it is a
/// valid login of the same account as the file, differs from it and was
/// refreshed later (`last_refresh`). Returns false only when such a login
/// could not be copied in, or the file could not be compared, so that the
/// caller keeps it; a leftover that is unreadable or not a login is not
/// worth keeping.
pub(crate) fn recover_login(secret_file: &Path, leftover: &Path, identity: Identity) -> bool {
    let Ok(candidate) = protected_read_native(leftover, identity.run_as) else {
        return true;
    };
    if aq_session_host::PrivateLaunchInput::validate_chatgpt_auth_json(&candidate).is_err() {
        return true;
    }
    let offer = || -> Result<()> {
        let _lock = lock_login(secret_file)?;
        let current = Zeroizing::new(protected_read(secret_file, 65536)?);
        let before: Value = serde_json::from_slice(&current)?;
        let after: Value = serde_json::from_slice(&candidate)?;
        let refreshed = |login: &Value| {
            let at = login["last_refresh"].as_str()?;
            chrono::DateTime::parse_from_rfc3339(at).ok()
        };
        if before["tokens"]["account_id"] == after["tokens"]["account_id"]
            && candidate.as_slice() != current.as_slice()
            && refreshed(&after) > refreshed(&before)
        {
            replace_login(secret_file, &candidate)?;
        }
        Ok(())
    };
    offer().is_ok()
}

/// Opens the native executable and checks its owner, mode, size and digest.
/// Keep the descriptor open until spawn: the child execs `/proc/self/fd/N`,
/// so replacing the path cannot substitute a different binary.
pub(crate) fn verify_native(
    native: &Path,
    digest: &Digest,
    identity: Identity,
) -> Result<std::fs::File> {
    let executable = open_native(native, identity)?;
    ensure!(
        file_digest(&executable)? == digest.as_str(),
        "native digest mismatch"
    );
    Ok(executable)
}

/// Opens `native` without following a symlink and checks the file, but not
/// its digest: a regular, executable file of at most 512 MiB, owned by
/// `identity.executable_owner` and not writable by group or others.
pub(crate) fn open_native(native: &Path, identity: Identity) -> Result<std::fs::File> {
    let executable = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(native)?;
    let metadata = executable.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == identity.executable_owner
            && metadata.mode() & 0o022 == 0
            && metadata.mode() & 0o111 != 0
            && metadata.len() <= 512 * 1024 * 1024,
        "native executable rejected"
    );
    Ok(executable)
}

/// The SHA-256 of `file` read from its current offset (the start, when it
/// was just opened), in lowercase hex as [`Digest::of`] gives it. It streams
/// the file instead of holding it in memory.
pub(crate) fn file_digest(file: &std::fs::File) -> Result<String> {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file.take(512 * 1024 * 1024 + 1), &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// The `codex exec` arguments before the `--config` overrides.
pub(crate) fn exec_arguments(
    sandbox: &str,
    model: &str,
    cd: &Path,
    output: &Path,
) -> Result<Vec<String>> {
    Ok(vec![
        "--no-daemon".to_owned(),
        "exec".into(),
        "--ignore-user-config".into(),
        "--ignore-rules".into(),
        "--ephemeral".into(),
        "--json".into(),
        "--color".into(),
        "never".into(),
        "--sandbox".into(),
        sandbox.into(),
        "--skip-git-repo-check".into(),
        "--model".into(),
        model.into(),
        "--cd".into(),
        cd.to_str().context("workspace UTF8")?.into(),
        "--output-last-message".into(),
        output.to_str().context("output UTF8")?.into(),
    ])
}

/// The `--config` override for a model choice's `service_tier` option, if it
/// has one.
pub(crate) fn service_tier(
    options: &std::collections::BTreeMap<String, Value>,
) -> Result<Option<String>> {
    let Some(tier) = options.get("service_tier").and_then(Value::as_str) else {
        return Ok(None);
    };
    ensure!(
        ["fast", "flex", "priority", "default", "auto"].contains(&tier),
        "unsupported service tier"
    );
    Ok(Some(format!("service_tier={}", quote(tier))))
}

/// The `--config` overrides every native run takes; callers append theirs.
/// Without `reasoning` Codex keeps the model's own effort.
pub(crate) fn exec_overrides(reasoning: Option<&str>) -> Vec<String> {
    let effort = reasoning.map(|r| format!("model_reasoning_effort={}", quote(r)));
    let fixed = [
        "approval_policy=\"never\"",
        "model_provider=\"openai\"",
        "forced_login_method=\"chatgpt\"",
        "cli_auth_credentials_store=\"file\"",
        "project_doc_max_bytes=0",
        "features.plugins=false",
        "features.apps=false",
        "features.shell_snapshot=false",
        "features.shell_snapshot_v2=false",
        "shell_environment_policy.inherit=\"none\"",
        "shell_environment_policy.set.PATH=\"/usr/local/bin:/usr/bin:/bin\"",
    ];
    effort.into_iter().chain(fixed.map(String::from)).collect()
}

/// `args`, then each override as `--config VALUE`, then `-` (prompt on stdin).
pub(crate) fn exec_finish(mut args: Vec<String>, overrides: Vec<String>) -> Vec<String> {
    for value in overrides {
        args.extend(["--config".into(), value]);
    }
    args.push("-".into());
    args
}

/// The native child: a clean environment, `home` as its Codex home, `cwd` as
/// its directory, stdin piped, its own session, and (in production) the
/// unprivileged identity with no new privileges and no core dumps.
pub(crate) fn native_command(
    executable: &std::fs::File,
    args: Vec<String>,
    home: &Path,
    cwd: &Path,
    identity: Identity,
) -> std::process::Command {
    use std::os::fd::AsRawFd;
    let fd = executable.as_raw_fd();
    let path = std::path::PathBuf::from(format!("/proc/self/fd/{fd}"));
    let mut command = std::process::Command::new(path);
    command
        .args(args)
        .env_clear()
        .env("HOME", home)
        .env("CODEX_HOME", home)
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped());
    let Identity {
        run_as,
        drop_privileges,
        ..
    } = identity;
    unsafe {
        command.pre_exec(move || {
            crate::adapter::new_session()?;
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0
                || (drop_privileges
                    && (libc::setgroups(0, std::ptr::null()) != 0
                        || libc::setgid(run_as) != 0
                        || libc::setuid(run_as) != 0))
                || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

/// Replaces the login tokens of each auth document in `text`.
pub(crate) fn redact_login(mut text: String, documents: &[&[u8]]) -> Result<String> {
    for doc in documents {
        let value: Value = serde_json::from_slice(doc)?;
        for key in ["id_token", "access_token", "refresh_token"] {
            if let Some(secret) = value["tokens"][key].as_str()
                && !secret.is_empty()
            {
                text = text.replace(secret, "[credential redacted]");
            }
        }
    }
    Ok(text)
}

fn protected_read_native(path: &Path, owner: u32) -> Result<Zeroizing<Vec<u8>>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.uid() == owner && meta.mode() & 0o077 == 0 && meta.len() <= 65536,
        "private native file rejected"
    );
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(65537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "private native file bound");
    Ok(bytes)
}

/// The child's final message at `path`: an owner-only file of `owner`, at
/// most 1 MiB of UTF-8, not blank and without NUL.
pub(crate) fn read_reply(path: &Path, owner: u32) -> Result<String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.uid() == owner && meta.mode() & 0o077 == 0 && meta.len() <= 1048576,
        "native reply bounds"
    );
    let mut bytes = Vec::new();
    file.take(1048577).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1048576, "native reply bounds");
    let body = String::from_utf8(bytes.to_vec()).context("native reply UTF8")?;
    ensure!(
        !body.trim().is_empty() && !body.contains('\0'),
        "native final reply missing"
    );
    Ok(body)
}

fn read_message(path: &Path) -> Result<String> {
    let mut body = read_reply(path, Identity::NATIVE.run_as)?;
    if body.len() > 16000 {
        let mut end = 15900;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
        body.push_str("\n[Reply shortened to fit the conversation.]");
    }
    Ok(body)
}
async fn publish(
    c: &Config,
    _directory: &Path,
    credential: &Path,
    body: &str,
    actions: &Path,
) -> Result<()> {
    let pool = sqlx::PgPool::connect(&c.database_url).await?;
    let existing: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM central_supervisor_replies WHERE dispatch_id=$1)",
    )
    .bind(c.central.dispatch_id)
    .fetch_one(&pool)
    .await?;
    if existing {
        return Ok(());
    }
    let mut refs = Vec::new();
    if let Ok(bytes) = protected_read_native(actions, Identity::NATIVE.run_as) {
        let text = std::str::from_utf8(&bytes)?;
        for line in text.lines() {
            if let Ok(id) = line.parse::<uuid::Uuid>()
                && refs.len() < 4
                && !refs.contains(&id)
            {
                refs.push(id);
            }
        }
    }
    let request = json!({"operation_id":uuid::Uuid::new_v4(),"message_id":uuid::Uuid::new_v4(),"dispatch_id":c.central.dispatch_id,"body":body,"attachments":refs.into_iter().map(|id|json!({"type":"action_receipt","operation_id":id})).collect::<Vec<_>>()});
    let token = Zeroizing::new(protected_read_native(credential, Identity::NATIVE.run_as)?);
    let mut bearer = reqwest::header::HeaderValue::from_str(&format!(
        "Bearer {}",
        std::str::from_utf8(&token)?
    ))?;
    bearer.set_sensitive(true);
    let url = crate::tool::origin(
        &c.subscription
            .as_ref()
            .context("subscription mode")?
            .server_origin,
    )?
    .join("v1/supervisor/conversation/replies")?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(8))
        .build()?;
    for _ in 0..3 {
        match client
            .post(url.clone())
            .header(reqwest::header::AUTHORIZATION, bearer.clone())
            .json(&request)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) if r.status().is_client_error() => anyhow::bail!("supervisor reply refused"),
            _ => tokio::time::sleep(std::time::Duration::from_millis(300)).await,
        }
    }
    anyhow::bail!("supervisor reply confirmation unavailable")
}

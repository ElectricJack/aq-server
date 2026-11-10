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

pub async fn native(
    c: &Config,
    directory: &Path,
    home: &Path,
    credential: &Path,
    startup: &[u8],
) -> Result<()> {
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
    let auth = Zeroizing::new(protected_read(&c.secret_file, 65536)?);
    aq_session_host::PrivateLaunchInput::validate_chatgpt_auth_json(&auth)?;
    let auth_path = home.join("auth.json");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&auth_path)?
        .write_all(&auth)?;
    std::os::unix::fs::chown(&auth_path, Some(10001), Some(10001))?;
    std::os::unix::fs::chown(home, Some(10001), Some(10001))?;
    let executable = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&c.native)?;
    let metadata = executable.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == 0
            && metadata.mode() & 0o022 == 0
            && metadata.mode() & 0o111 != 0
            && metadata.len() <= 512 * 1024 * 1024,
        "native executable rejected"
    );
    let mut bytes = Vec::new();
    (&executable)
        .take(512 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        Digest::of(&bytes) == c.native_digest,
        "native digest mismatch"
    );
    drop(bytes);
    use std::os::fd::AsRawFd;
    let fd = executable.as_raw_fd();
    let path = std::path::PathBuf::from(format!("/proc/self/fd/{fd}"));
    let output = home.join("last-message.txt");
    let actions = home.join("action-receipts");
    let mut args = vec![
        "--no-daemon".to_owned(),
        "exec".into(),
        "--ignore-user-config".into(),
        "--ignore-rules".into(),
        "--ephemeral".into(),
        "--json".into(),
        "--color".into(),
        "never".into(),
        "--sandbox".into(),
        "danger-full-access".into(),
        "--skip-git-repo-check".into(),
        "--model".into(),
        c.model.model.as_str().into(),
        "--cd".into(),
        c.workspace.to_str().context("workspace UTF8")?.into(),
        "--output-last-message".into(),
        output.to_str().context("output UTF8")?.into(),
    ];
    let quote = |v: &str| serde_json::to_string(v).expect("text serializes");
    let mut overrides = vec![
        format!(
            "model_reasoning_effort={}",
            quote(c.model.reasoning.as_str())
        ),
        "approval_policy=\"never\"".into(),
        "model_provider=\"openai\"".into(),
        "forced_login_method=\"chatgpt\"".into(),
        "cli_auth_credentials_store=\"file\"".into(),
        "project_doc_max_bytes=0".into(),
        "features.plugins=false".into(),
        "features.apps=false".into(),
        "features.shell_snapshot=false".into(),
        "features.shell_snapshot_v2=false".into(),
        "shell_environment_policy.inherit=\"none\"".into(),
        "shell_environment_policy.set.PATH=\"/usr/local/bin:/usr/bin:/bin\"".into(),
        format!("developer_instructions={}", quote(&settings.instructions)),
    ];
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
    if let Some(tier) = settings.options.get("service_tier").and_then(Value::as_str) {
        ensure!(
            ["fast", "flex", "priority", "default", "auto"].contains(&tier),
            "unsupported service tier"
        );
        overrides.push(format!("service_tier={}", quote(tier)));
    }
    for value in overrides {
        args.extend(["--config".into(), value]);
    }
    args.push("-".into());
    let mut command = std::process::Command::new(path);
    command
        .args(args)
        .env_clear()
        .env("HOME", home)
        .env("CODEX_HOME", home)
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .current_dir(&c.workspace)
        .stdin(std::process::Stdio::piped());
    unsafe {
        command.pre_exec(move || {
            crate::adapter::new_session()?;
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0
                || libc::setgroups(0, std::ptr::null()) != 0
                || libc::setgid(10001) != 0
                || libc::setuid(10001) != 0
                || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    child
        .stdin
        .take()
        .context("native stdin")?
        .write_all(startup)?;
    let status = tokio::task::spawn_blocking(move || child.wait()).await??;
    // Retain only a validated refresh of this subscription account inside this
    // central container; never write the operator's original host auth document.
    if let Ok(refreshed) = protected_read_native(&auth_path)
        && aq_session_host::PrivateLaunchInput::validate_chatgpt_auth_json(&refreshed).is_ok()
    {
        let original: Value = serde_json::from_slice(&auth)?;
        let current: Value = serde_json::from_slice(&refreshed)?;
        if original["tokens"]["account_id"] == current["tokens"]["account_id"]
            && refreshed.as_slice() != auth.as_slice()
        {
            let temp = c
                .secret_file
                .with_extension(format!("refresh-{}", uuid::Uuid::new_v4()));
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?
                .write_all(&refreshed)?;
            std::fs::rename(temp, &c.secret_file)?;
        }
    }
    let _ = std::fs::remove_file(&auth_path);
    ensure!(status.success(), "native supervisor turn failed");
    let mut text = read_message(&output)?;
    let aq_token = Zeroizing::new(protected_read_native(credential)?);
    text = text.replace(std::str::from_utf8(&aq_token)?, "[credential redacted]");
    for doc in [
        &auth,
        &Zeroizing::new(protected_read(&c.secret_file, 65536)?),
    ] {
        let value: Value = serde_json::from_slice(doc)?;
        for key in ["id_token", "access_token", "refresh_token"] {
            if let Some(secret) = value["tokens"][key].as_str()
                && !secret.is_empty()
            {
                text = text.replace(secret, "[credential redacted]");
            }
        }
    }

    publish(c, directory, credential, &text, &actions).await
}
fn protected_read_native(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.uid() == 10001 && meta.mode() & 0o077 == 0 && meta.len() <= 65536,
        "private native file rejected"
    );
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(65537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "private native file bound");
    Ok(bytes)
}
fn read_message(path: &Path) -> Result<String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.uid() == 10001 && meta.mode() & 0o077 == 0 && meta.len() <= 1048576,
        "native reply bounds"
    );
    let mut bytes = Vec::new();
    file.take(1048577).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1048576, "native reply bounds");
    let mut body = String::from_utf8(bytes.to_vec()).context("native reply UTF8")?;
    ensure!(
        !body.trim().is_empty() && !body.contains('\0'),
        "native final reply missing"
    );
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
    if let Ok(bytes) = protected_read_native(actions) {
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
    let token = Zeroizing::new(protected_read_native(credential)?);
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

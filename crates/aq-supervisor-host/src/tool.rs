//! A bounded native tool client. Only file paths, never bearer bytes, are in argv.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::io::{Read, Write};
use zeroize::Zeroizing;
pub fn origin(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value)?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/",
        "invalid supervisor machine origin"
    );
    Ok(url)
}
pub async fn run(args: &[String]) -> Result<()> {
    let name = args
        .first()
        .context("usage: aq-supervisor-host tool commands [NAME] | NAME < request.json")?;
    ensure!(
        name.len() <= 128
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b"._-".contains(&b)),
        "invalid command name"
    );
    let url = origin(&std::env::var("AQ_SUPERVISOR_ORIGIN")?)?;
    let path = std::path::PathBuf::from(std::env::var("AQ_SUPERVISOR_CREDENTIAL_FILE")?);
    let token = Zeroizing::new(crate::config::protected_read(&path, 70)?);
    let mut bearer = reqwest::header::HeaderValue::from_str(&format!(
        "Bearer {}",
        std::str::from_utf8(&token)?
    ))?;
    bearer.set_sensitive(true);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let mut operation = None;
    let request = if name == "commands" {
        client.get(url.join("api/v1/commands")?)
    } else {
        let mut bytes = Vec::new();
        std::io::stdin().take(131073).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 131072, "tool request byte bound");
        let body: aq_command_registry::CommandRequest = if bytes.is_empty() {
            Default::default()
        } else {
            serde_json::from_slice(&bytes)?
        };
        operation = body.operation_id;
        client
            .post(url.join(&format!("api/v1/commands/{name}"))?)
            .json(&body)
    };
    let response = request
        .header(reqwest::header::AUTHORIZATION, bearer)
        .send()
        .await
        .context("supervisor tool transport failed")?;
    let status = response.status();
    ensure!(
        response.content_length().is_none_or(|n| n <= 2097152),
        "tool response byte bound"
    );
    let bytes = response.bytes().await?;
    ensure!(bytes.len() <= 2097152, "tool response byte bound");
    let value: Value = serde_json::from_slice(&bytes)?;
    if status.is_success()
        && matches!(name.as_str(), "task.create" | "task.graph.create")
        && value["data"]["disposition"] == "confirmed"
        && let Some(id) = operation
        && let Ok(path) = std::env::var("AQ_SUPERVISOR_ACTIONS_FILE")
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut out = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o600)
            .open(path)?;
        ensure!(out.metadata()?.len() < 4096, "action receipt bound");
        writeln!(out, "{id}")?;
    }
    let output = if name == "commands" && args.len() == 2 {
        find_command(&value, &args[1]).context("command is not available")?
    } else {
        &value
    };
    serde_json::to_writer(std::io::stdout(), output)?;
    println!();
    ensure!(status.is_success(), "AQ refused the supervisor command");
    Ok(())
}
fn find_command<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    if value["name"] == name {
        return Some(value);
    }
    match value {
        Value::Array(items) => items.iter().find_map(|v| find_command(v, name)),
        Value::Object(fields) => fields.values().find_map(|v| find_command(v, name)),
        _ => None,
    }
}

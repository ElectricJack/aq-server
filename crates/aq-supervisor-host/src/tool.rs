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
        describe(find_command(&value, &args[1]).context("command is not available")?)?
    } else {
        value
    };
    serde_json::to_writer(std::io::stdout(), &output)?;
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

/// Discovery lists names, not input schemas. Supply the actual typed payload
/// and outer command envelope so native callers never have to guess fields.
fn describe(command: &Value) -> Result<Value> {
    let name = command["name"].as_str().context("command name")?;
    let protocol: Value =
        serde_json::from_str(include_str!("../../../protocol/commands/v1/schema.json"))?;
    let actions: Value = serde_json::from_str(include_str!(
        "../../../protocol/dashboard-actions/v1/schema.json"
    ))?;
    let mut request = protocol["$defs"]["CommandRequest"].clone();
    let catalog = aq_server::commands::document();
    let entry = find_command(&catalog, name).context("installed command schema")?;
    let mut params = entry["principals"]["supervisor"]["params_schema"].clone();
    let action = actions["$defs"]["ActionRequest"]["oneOf"]
        .as_array()
        .and_then(|variants| {
            variants
                .iter()
                .find(|v| v["properties"]["kind"]["const"] == name)
        });
    if let Some(action) = action {
        params = action["properties"]["payload"].clone();
        request["properties"]["subject"] = actions["$defs"]["EntityRef"].clone();
        request["properties"]["expected_versions"] =
            actions["$defs"]["ActionRequest"]["properties"]["expected_versions"].clone();
        request["required"] =
            serde_json::json!(["operation_id", "subject", "expected_versions", "params"]);
    }
    request["properties"]["params"] = params;
    Ok(
        serde_json::json!({"command":command,"request_schema":request,"instructions":"Submit this CommandRequest JSON on stdin. params is the exact action payload (including operation_id when required), not an arbitrary option map. For mutations use the SAME fresh UUID for outer operation_id and params.operation_id. subject is {type,id,revision}; expected_versions is keyed by project or task, not project_revision. For task.create use subject.type=project, subject.id=the actual project UUID, subject.revision=current project revision, expected_versions.project=that revision, params.expected_project_revision=that revision. Do not add fields absent from the closed params schema. On uncertain transport reuse the exact body/UUID and inspect operation.show; never create a replacement."}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mutation_help_supplies_the_payload_and_a_valid_command_envelope() {
        let help = describe(&serde_json::json!({"name":"task.create","kind":"mutation"})).unwrap();
        assert!(
            help["request_schema"]["properties"]["params"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "operation_id")
        );
        let action: Value = serde_json::from_str(include_str!(
            "../../../protocol/dashboard-actions/v1/fixtures/request-task-create.json"
        ))
        .unwrap();
        let command:aq_command_registry::CommandRequest=serde_json::from_value(serde_json::json!({"operation_id":action["operation_id"],"subject":action["subject"],"expected_versions":action["expected_versions"],"params":action["payload"]})).unwrap();
        let registry = aq_server::commands::registry();
        let spec = registry.get("task.create").unwrap();
        let forward = aq_command_registry::forward(
            spec,
            spec.route(aq_command_registry::Principal::Supervisor)
                .unwrap(),
            &command,
            None,
            uuid::Uuid::new_v4(),
        )
        .unwrap();
        assert_eq!(forward.body, Some(action));
    }
}

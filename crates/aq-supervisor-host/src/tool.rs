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
/// The machine origin and the turn's bearer, from the turn environment.
pub(crate) fn machine() -> Result<(reqwest::Url, Zeroizing<Vec<u8>>)> {
    let url = origin(&std::env::var("AQ_SUPERVISOR_ORIGIN")?)?;
    let path = std::path::PathBuf::from(std::env::var("AQ_SUPERVISOR_CREDENTIAL_FILE")?);
    Ok((
        url,
        Zeroizing::new(crate::config::protected_read(&path, 70)?),
    ))
}
pub async fn run(args: &[String]) -> Result<()> {
    let name = args.first().context(
        "usage: aq-supervisor-host tool commands [NAME] | attachment-text ID | NAME < request.json",
    )?;
    ensure!(
        name.len() <= 128
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b"._-".contains(&b)),
        "invalid command name"
    );
    let (url, token) = machine()?;
    let mut bearer = reqwest::header::HeaderValue::from_str(&format!(
        "Bearer {}",
        std::str::from_utf8(&token)?
    ))?;
    bearer.set_sensitive(true);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let ToolRequest {
        request,
        bound,
        operation,
    } = prepare(&client, &url, name, args)?;
    let response = request
        .header(reqwest::header::AUTHORIZATION, bearer)
        .send()
        .await
        .context("supervisor tool transport failed")?;
    let status = response.status();
    ensure!(
        response.content_length().is_none_or(|n| n <= bound),
        "tool response byte bound"
    );
    let bytes = response.bytes().await?;
    ensure!(bytes.len() as u64 <= bound, "tool response byte bound");
    let value: Value = serde_json::from_slice(&bytes)?;
    if status.is_success() && matches!(name.as_str(), "task.create" | "task.graph.create") {
        record_action(&value, operation)?;
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
struct ToolRequest {
    request: reqwest::RequestBuilder,
    bound: u64,
    operation: Option<uuid::Uuid>,
}

fn prepare(
    client: &reqwest::Client,
    url: &reqwest::Url,
    name: &str,
    args: &[String],
) -> Result<ToolRequest> {
    let mut operation = None;
    // Owner attachment contents are larger than command responses.
    let mut bound = 2097152;
    let request = if name == "commands" {
        client.get(url.join("api/v1/commands")?)
    } else if name == "attachment-text" {
        let id: uuid::Uuid = args
            .get(1)
            .context("usage: aq-supervisor-host tool attachment-text ID")?
            .parse()
            .context("attachment ID is not a UUID")?;
        bound = aq_supervisor_control::delivery::MAX_DELIVERY_BYTES as u64;
        client.get(url.join(&format!("v1/supervisor/conversation/attachments/{id}/text"))?)
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
    Ok(ToolRequest {
        request,
        bound,
        operation,
    })
}

fn record_action(value: &Value, operation: Option<uuid::Uuid>) -> Result<()> {
    if value["data"]["disposition"] != "confirmed" {
        return Ok(());
    }
    let Some(id) = operation else {
        return Ok(());
    };
    let Ok(path) = std::env::var("AQ_SUPERVISOR_ACTIONS_FILE") else {
        return Ok(());
    };
    use std::os::unix::fs::OpenOptionsExt;
    let mut out = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(path)?;
    ensure!(out.metadata()?.len() < 4096, "action receipt bound");
    writeln!(out, "{id}")?;
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
    let mut request = protocol["$defs"]["CommandRequest"].clone();
    let spec = aq_server::commands::registry()
        .get(name)
        .context("installed command schema")?;
    let route = spec
        .route(aq_command_registry::Principal::Supervisor)
        .context("installed supervisor command route")?;
    let instructions = if route.body == aq_command_registry::Body::Action {
        describe_action(&mut request, name)?
    } else {
        let catalog = aq_server::commands::document();
        let entry = find_command(&catalog, name).context("installed command schema")?;
        request["properties"]["params"] =
            entry["principals"]["supervisor"]["params_schema"].clone();
        "Submit one CommandRequest JSON object on stdin. Use the params schema for this command."
            .into()
    };
    Ok(serde_json::json!({"command":command,"request_schema":request,"instructions":instructions}))
}

fn describe_action(request: &mut Value, name: &str) -> Result<String> {
    use serde_json::json;

    let actions: Value = serde_json::from_str(include_str!(
        "../../../protocol/dashboard-actions/v1/schema.json"
    ))?;
    let action = &actions["$defs"]["ActionRequest"];
    let variant = action["oneOf"]
        .as_array()
        .and_then(|variants| {
            variants
                .iter()
                .find(|v| v["properties"]["kind"]["const"] == name)
        })
        .context("installed action payload schema")?;
    let mut params = variant["properties"]["payload"].clone();
    // Action forwarding accepts either operation ID location, then supplies
    // the action payload's copy when the caller used only the command envelope.
    params["required"]
        .as_array_mut()
        .context("action payload required fields")?
        .retain(|field| field != "operation_id");
    request["description"] = json!("CommandRequest accepted by the supervisor's action route.");
    request["required"] = json!(["params"]);
    request["anyOf"] = json!([
        {"required":["operation_id"]},
        {"properties":{"params":{"required":["operation_id"]}}}
    ]);
    request["properties"]["subject"] = actions["$defs"]["EntityRef"].clone();
    request["properties"]["expected_versions"] = action["properties"]["expected_versions"].clone();

    let mut instructions = String::from(concat!(
        "Submit one CommandRequest JSON object on stdin. Put a fresh UUID in the outer operation_id. ",
        "AQ copies it into params.operation_id when that field is omitted. If you supply both, use the same UUID. ",
        "On uncertain transport, reuse the exact request and UUID and inspect operation.show."
    ));
    if let Some(rule) = aq_command_registry::subject_rule(name) {
        describe_subject(request, &mut params, rule, &mut instructions);
    } else {
        request["required"] = json!(["params", "subject", "expected_versions"]);
    }
    request["properties"]["params"] = params;
    Ok(instructions)
}

fn describe_subject(
    request: &mut Value,
    params: &mut Value,
    rule: aq_command_registry::SubjectRule,
    instructions: &mut String,
) {
    use aq_command_registry::SubjectId;
    use serde_json::json;

    let mut explicit = json!({"title":"Explicit subject","required":["subject"]});
    let mut derived = json!({"title":"Derived subject","not":{"required":["subject"]}});
    request["properties"]["subject"]["properties"]["type"]["const"] = json!(rule.entity);
    if let SubjectId::Param(id) = rule.id {
        params["properties"][id] = rule.describe()["id_param_schema"].clone();
        explicit["properties"]["params"] = json!({"not":{"required":[id]}});
        derived["properties"]["params"] = json!({"required":[id]});
        instructions.push_str(&format!(
            " For a derived subject, omit subject and include params.{id}. For an explicit subject, put the ID in subject.id and omit params.{id}."
        ));
    } else {
        instructions.push_str(" Omit subject to let AQ derive it from the action params.");
    }
    if let Some((field, key)) = rule.revision {
        explicit["required"] = json!(["subject", "expected_versions"]);
        explicit["properties"]["subject"] = json!({"required":["revision"]});
        explicit["properties"]["expected_versions"] = json!({"required":[key]});
        instructions.push_str(&format!(
            " Put the current revision as a decimal string in params.{field}. AQ derives expected_versions when omitted with a derived subject. For an explicit subject, use type {} and put the same revision in subject.revision and expected_versions.{key}.",
            rule.entity
        ));
    }
    request["oneOf"] = json!([derived, explicit]);
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mutation_help_supplies_the_payload_and_a_valid_command_envelope() {
        let help = describe(&serde_json::json!({"name":"task.create","kind":"mutation"})).unwrap();
        assert_eq!(
            help["request_schema"]["properties"]["params"]["properties"]["operation_id"]["format"],
            "uuid"
        );
        let action: Value = serde_json::from_str(include_str!(
            "../../../protocol/dashboard-actions/v1/fixtures/request-task-create.json"
        ))
        .unwrap();
        let command:aq_command_registry::CommandRequest=serde_json::from_value(serde_json::json!({"operation_id":action["operation_id"],"subject":action["subject"],"expected_versions":action["expected_versions"],"params":action["payload"]})).unwrap();
        let registry = aq_server::commands::registry();
        let spec = registry.get("task.create").unwrap();
        let forward = spec
            .forward(
                spec.route(aq_command_registry::Principal::Supervisor)
                    .unwrap(),
                &command,
                None,
            )
            .unwrap();
        assert_eq!(forward.body, Some(action));
    }

    #[cfg(feature = "supervisor_full_access")]
    #[test]
    fn retry_help_describes_both_requests_accepted_by_the_scoped_owner_bridge() {
        use aq_command_registry::{CommandRequest, Principal};
        use serde_json::json;
        use uuid::Uuid;

        let help = describe(&json!({"name":"task.retry","kind":"mutation"})).unwrap();
        let request = &help["request_schema"];
        let params = &request["properties"]["params"];
        assert_eq!(
            params["required"],
            json!(["project_id", "expected_version"])
        );
        assert_eq!(params["properties"]["task_id"]["format"], "uuid");
        assert_eq!(request["required"], json!(["params"]));
        assert_eq!(request["anyOf"][0]["required"], json!(["operation_id"]));
        assert_eq!(request["oneOf"][0]["not"]["required"], json!(["subject"]));
        assert_eq!(
            request["oneOf"][0]["properties"]["params"]["required"],
            json!(["task_id"])
        );
        assert_eq!(
            request["oneOf"][1]["properties"]["params"]["not"]["required"],
            json!(["task_id"])
        );

        let project = Uuid::from_u128(1);
        let task = Uuid::from_u128(2);
        let operation = Uuid::from_u128(3);
        let reason = "The required check has recovered.";
        // The first request follows the recipe published in a hold notice.
        let derived = json!({"operation_id":operation,"reason":reason,
            "params":{"project_id":project,"task_id":task,"expected_version":"17"}});
        let explicit = json!({"operation_id":operation,"reason":reason,
            "subject":{"type":"task","id":task,"revision":"17"},
            "expected_versions":{"task":"17"},
            "params":{"operation_id":operation,"project_id":project,"expected_version":"17"}});
        let expected = json!({"kind":"task.retry","operation_id":operation,"reason":reason,
            "subject":explicit["subject"],"expected_versions":explicit["expected_versions"],
            "payload":explicit["params"]});
        let spec = aq_server::commands::registry().get("task.retry").unwrap();
        let route = spec.route(Principal::Supervisor).unwrap();
        for packet in [derived, explicit] {
            let request: CommandRequest = serde_json::from_value(packet).unwrap();
            let forward = spec.forward(route, &request, None).unwrap();
            assert_eq!(
                forward.uri,
                format!("{}/api/v1/actions", aq_command_registry::OWNER_PREFIX)
            );
            assert_eq!(forward.body.as_ref(), Some(&expected));
        }
    }
}

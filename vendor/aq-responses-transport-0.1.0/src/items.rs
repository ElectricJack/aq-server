//! Text/local-tool-only stateless subset. Server-side item/file handles and
//! hosted/remote tools require a separate explicitly scoped qualification.
use super::*;
use serde_json::Value;
fn text_content(value: &Value, reasoning: bool) -> Result<()> {
    if value.is_string() {
        return Ok(());
    }
    for content in value.as_array().context("Responses content shape")? {
        let item = content
            .as_object()
            .context("Responses content item shape")?;
        let kind = item
            .get("type")
            .and_then(Value::as_str)
            .context("Responses content kind absent")?;
        ensure!(
            if reasoning {
                matches!(kind, "summary_text" | "reasoning_text" | "text")
            } else {
                matches!(kind, "input_text" | "output_text")
            },
            "unsupported Responses media/reference content"
        );
        ensure!(
            item.get("text").is_some_and(Value::is_string)
                && item.keys().all(|key| matches!(
                    key.as_str(),
                    "type" | "text" | "annotations" | "logprobs"
                )),
            "Responses text content fields"
        );
    }
    Ok(())
}
pub(super) fn validate(body: &serde_json::Map<String, Value>) -> Result<()> {
    let inputs = body
        .get("input")
        .and_then(Value::as_array)
        .context("Responses inputs absent")?;
    for input in inputs {
        let item = input.as_object().context("Responses input item shape")?;
        let kind = item
            .get("type")
            .and_then(Value::as_str)
            .context("Responses explicit input kind required")?;
        let allowed: &[&str] = match kind {
            "message" => &[
                "type",
                "id",
                "role",
                "content",
                "status",
                "phase",
                "encrypted_content",
                "internal_chat_message_metadata_passthrough",
            ],
            "reasoning" => &[
                "type",
                "id",
                "summary",
                "content",
                "encrypted_content",
                "internal_chat_message_metadata_passthrough",
            ],
            "function_call" => &[
                "type",
                "id",
                "status",
                "call_id",
                "name",
                "namespace",
                "arguments",
                "encrypted_content",
                "encrypted_function_args",
                "internal_chat_message_metadata_passthrough",
            ],
            "custom_tool_call" => &[
                "type",
                "id",
                "status",
                "call_id",
                "name",
                "namespace",
                "input",
                "encrypted_content",
                "internal_chat_message_metadata_passthrough",
            ],
            "function_call_output" | "custom_tool_call_output" => &[
                "type",
                "id",
                "call_id",
                "name",
                "namespace",
                "output",
                "internal_chat_message_metadata_passthrough",
            ],
            _ => anyhow::bail!("unsupported Responses input kind/reference"),
        };
        ensure!(
            item.keys().all(|key| allowed.contains(&key.as_str())),
            "unsupported Responses input fields"
        );
        match kind {
            "message" => {
                ensure!(
                    item.get("role")
                        .and_then(Value::as_str)
                        .is_some_and(|r| matches!(
                            r,
                            "system" | "developer" | "user" | "assistant"
                        )),
                    "Responses message role"
                );
                text_content(
                    item.get("content")
                        .context("Responses message content absent")?,
                    false,
                )?;
            }
            "reasoning" => {
                text_content(
                    item.get("summary")
                        .context("Responses reasoning summary absent")?,
                    true,
                )?;
                if let Some(content) = item.get("content").filter(|v| !v.is_null()) {
                    text_content(content, true)?;
                }
            }
            "function_call" | "custom_tool_call" => {
                let argument = if kind == "function_call" {
                    "arguments"
                } else {
                    "input"
                };
                ensure!(
                    item.get(argument).is_some_and(Value::is_string)
                        && item.get("name").is_some_and(Value::is_string)
                        && item.get("call_id").is_some_and(Value::is_string),
                    "Responses local call body"
                );
            }
            _ => {
                text_content(
                    item.get("output")
                        .context("Responses local output absent")?,
                    false,
                )?;
            }
        }
    }
    if let Some(tools) = body.get("tools").filter(|v| !v.is_null()) {
        for tool in tools.as_array().context("Responses local tools shape")? {
            ensure!(
                tool.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| matches!(kind, "function" | "custom"))
                    && tool.get("name").is_some_and(Value::is_string),
                "unsupported hosted/remote Responses tool"
            );
        }
    }
    Ok(())
}

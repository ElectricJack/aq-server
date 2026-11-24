//! A stdio MCP façade over the scoped supervisor machine client.
use crate::tool;
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};

const MAX_FRAME_BYTES: u64 = 262_144;
const PROTOCOL_VERSION: &str = "2024-11-05";

pub async fn run() -> Result<()> {
    Bridge::default()
        .serve(BufReader::new(tokio::io::stdin()), tokio::io::stdout())
        .await
}

#[derive(Default)]
struct Bridge {
    initialized: bool,
    #[cfg(all(feature = "supervisor_full_access", feature = "delivery_playbook"))]
    repositories: crate::repo::SessionRepositories,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Discover {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Describe {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    name: String,
    request: aq_command_registry::CommandRequest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Attachment {
    id: uuid::Uuid,
}

impl Bridge {
    async fn serve(
        &mut self,
        mut input: impl AsyncBufRead + Unpin,
        mut output: impl AsyncWrite + Unpin,
    ) -> Result<()> {
        let mut frame = Vec::new();
        loop {
            frame.clear();
            let read = (&mut input)
                .take(MAX_FRAME_BYTES + 1)
                .read_until(b'\n', &mut frame)
                .await?;
            if read == 0 {
                return Ok(());
            }
            anyhow::ensure!(read as u64 <= MAX_FRAME_BYTES, "MCP frame byte bound");
            let response = match serde_json::from_slice::<Value>(&frame) {
                Ok(request) => self.handle(request).await,
                Err(_) => Some(error(Value::Null, -32700, "Parse error")),
            };
            if let Some(response) = response {
                let mut line = serde_json::to_vec(&response)?;
                line.push(b'\n');
                output.write_all(&line).await?;
                output.flush().await?;
            }
        }
    }

    async fn handle(&mut self, request: Value) -> Option<Value> {
        let id = request.get("id").cloned();
        let method = request["method"].as_str();
        if request["jsonrpc"] != "2.0"
            || method.is_none()
            || id
                .as_ref()
                .is_some_and(|id| !id.is_string() && !id.is_i64() && !id.is_u64())
        {
            return Some(error(Value::Null, -32600, "Invalid request"));
        }
        let id = id?;
        let result = match method? {
            "initialize" => {
                let version = request["params"]["protocolVersion"].as_str();
                let version = match version {
                    Some(version @ ("2025-03-26" | "2025-06-18" | "2025-11-25")) => version,
                    Some(_) => PROTOCOL_VERSION,
                    None => return Some(error(id, -32602, "Missing protocol version")),
                };
                self.initialized = true;
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "aq", "version": env!("CARGO_PKG_VERSION")}
                })
            }
            "ping" => json!({}),
            "tools/list" if self.initialized => json!({"tools": definitions()}),
            "tools/call" if self.initialized => {
                let params = &request["params"];
                let Some(name) = params["name"].as_str() else {
                    return Some(error(id, -32602, "Missing tool name"));
                };
                if !definitions().iter().any(|tool| tool["name"] == name) {
                    return Some(error(id, -32602, "Unknown tool"));
                }
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match self.call(name, args).await {
                    Ok((value, succeeded)) => tool_result(&value.to_string(), !succeeded),
                    Err(CallError::Arguments) => tool_result("Invalid tool arguments.", true),
                    Err(CallError::Failed(message)) => tool_result(message, true),
                }
            }
            "tools/call" | "tools/list" => return Some(error(id, -32002, "Initialize first")),
            _ => return Some(error(id, -32601, "Method not found")),
        };
        Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
    }

    async fn call(
        &mut self,
        name: &str,
        args: Value,
    ) -> std::result::Result<(Value, bool), CallError> {
        let response = match name {
            "aq_discover" => {
                let _: Discover = parse(args)?;
                tool::execute("commands", None, &[]).await
            }
            "aq_describe" => {
                let Describe { name } = parse(args)?;
                tool::execute("commands", Some(&name), &[]).await
            }
            "aq_command" => {
                let Command { name, request } = parse(args)?;
                if matches!(name.as_str(), "commands" | "attachment-text") {
                    return Err(CallError::Arguments);
                }
                let body = serde_json::to_vec(&request).map_err(|_| CallError::Arguments)?;
                if body.len() > tool::MAX_REQUEST_BYTES {
                    return Err(CallError::Arguments);
                }
                tool::execute(&name, None, &body).await
            }
            "aq_attachment" => {
                let Attachment { id } = parse(args)?;
                tool::execute("attachment-text", Some(&id.to_string()), &[]).await
            }
            #[cfg(all(feature = "supervisor_full_access", feature = "delivery_playbook"))]
            "repo_sync" => {
                let args = parse::<crate::repo::SyncArguments>(args)?;
                return self
                    .repositories
                    .sync(args.project_id)
                    .await
                    .map(|value| (value, true))
                    .map_err(|_| CallError::Failed("AQ could not sync the project repository."));
            }
            #[cfg(all(feature = "supervisor_full_access", feature = "delivery_playbook"))]
            "repo_read" => {
                let args = parse::<crate::repo::ReadArguments>(args)?;
                return self.repositories.read(args).await
                    .map(|value| (value, true))
                    .map_err(|_| CallError::Failed("AQ could not read that repository path and ref. Sync the project first."));
            }
            _ => return Err(CallError::Arguments),
        };
        response.map_err(|_| CallError::Failed("AQ tool did not return a result. For an uncertain mutation, use operation.show before retrying."))
    }
}

enum CallError {
    Arguments,
    Failed(&'static str),
}

fn parse<T: serde::de::DeserializeOwned>(value: Value) -> std::result::Result<T, CallError> {
    serde_json::from_value(value).map_err(|_| CallError::Arguments)
}

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

fn tool_result(text: &str, is_error: bool) -> Value {
    json!({"content":[{"type":"text","text":text}],"isError":is_error})
}

fn definitions() -> Vec<Value> {
    let tool = |name, description, properties: Value, required: &[&str]| {
        json!({
            "name": name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false
            }
        })
    };
    let tools = vec![
        tool(
            "aq_discover",
            "List AQ commands available to this supervisor.",
            json!({}),
            &[],
        ),
        tool(
            "aq_describe",
            "Get the request schema and usage for a scoped AQ command.",
            json!({"name":{"type":"string"}}),
            &["name"],
        ),
        tool(
            "aq_command",
            "Call a scoped AQ command. Use aq_describe for its request schema.",
            json!({"name":{"type":"string"},"request":{"type":"object"}}),
            &["name", "request"],
        ),
        tool(
            "aq_attachment",
            "Read text from an owner conversation attachment by ID.",
            json!({"id":{"type":"string","format":"uuid"}}),
            &["id"],
        ),
    ];
    #[cfg(all(feature = "supervisor_full_access", feature = "delivery_playbook"))]
    let tools = {
        let mut tools = tools;
        tools.extend([
            tool("repo_sync", "Fetch the scoped project's repository for read-only inspection.",
                json!({"project_id":{"type":"string","format":"uuid"}}), &["project_id"]),
            tool("repo_read", "List a synced directory or read a tracked UTF-8 file. Use an empty path for the root. Ref defaults to main and can be aq/<task-id>.",
                json!({"project_id":{"type":"string","format":"uuid"},"path":{"type":"string"},"ref":{"type":"string"}}), &["project_id","path"]),
        ]);
        tools
    };
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stdio_negotiates_without_emitting_a_notification_response() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":0,"method":"tools/list"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":"list","method":"tools/list"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"aq_attachment","arguments":{"id":"/etc/passwd"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"read_file"}}"#,
            "\n",
        );
        let mut output = Vec::new();
        Bridge::default()
            .serve(input.as_bytes(), &mut output)
            .await
            .unwrap();
        let lines: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0]["error"]["code"], -32002);
        assert_eq!(lines[1]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(lines[2]["id"], "list");
        let names: Vec<_> = lines[2]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            &names[..4],
            ["aq_discover", "aq_describe", "aq_command", "aq_attachment"]
        );
        assert_eq!(
            names.contains(&"repo_sync"),
            cfg!(all(
                feature = "supervisor_full_access",
                feature = "delivery_playbook"
            ))
        );
        assert_eq!(names.contains(&"repo_read"), names.contains(&"repo_sync"));
        assert_eq!(lines[3]["result"]["isError"], true);
        assert_eq!(lines[4]["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn rejects_an_oversized_frame_without_parsing_it() {
        let input = vec![b' '; MAX_FRAME_BYTES as usize + 1];
        let mut output = Vec::new();
        assert!(
            Bridge::default()
                .serve(input.as_slice(), &mut output)
                .await
                .is_err()
        );
        assert!(output.is_empty());
    }
}

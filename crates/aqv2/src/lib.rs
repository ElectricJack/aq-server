//! Human command translation. Domain authority and mutation recovery stay on the server.
mod actions;
mod chat;
pub mod cli;
mod client;
mod json;
pub mod wire;
use aq_human_http::Client;
use clap::Parser;
use serde_json::{Value, json};
use uuid::Uuid;

pub type Result<T> = std::result::Result<T, Value>;
pub fn error(code: &str, message: &str) -> Value {
    json!({"code":code,"message":message})
}
pub fn unsupported(operation: &str) -> Value {
    error(
        "unsupported",
        &format!("{operation} is unsupported by the published server contract"),
    )
}
pub fn with_operation(mut error: Value, id: Uuid) -> Value {
    error["operation_id"] = json!(id);
    error["recovery"] = json!(format!(
        "aqv2 --origin ORIGIN --cf-access-token-file TOKEN_FILE operations show {id}"
    ));
    error
}
pub async fn execute(http: Client, args: &[&str]) -> Result<Value> {
    let commands =
        cli::Commands::try_parse_from(std::iter::once("aqv2").chain(args.iter().copied()))
            .map_err(|_| {
                error(
                    "invalid_arguments",
                    "Invalid command arguments; use aqv2 --help",
                )
            })?;
    client::execute(&http, &commands.command).await
}
pub async fn run(cli: cli::Cli) -> Result<Value> {
    if let Some(operation) = cli.commands.command.unsupported() {
        return Err(unsupported(operation));
    }
    let origin = cli
        .origin
        .ok_or_else(|| error("invalid_arguments", "--origin is required"))?;
    let token = cli
        .cf_access_token_file
        .ok_or_else(|| error("invalid_arguments", "--cf-access-token-file is required"))?;
    let http = Client::new(
        &origin,
        aq_human_http::Authentication::CfAccessTokenFile(token),
        aq_human_http::Limits {
            max_request_bytes: 65536,
            ..Default::default()
        },
    )
    .map_err(client::http_error)?;
    client::execute(&http, &cli.commands.command).await
}

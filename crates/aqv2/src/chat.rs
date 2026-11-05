use crate::{
    Result, actions,
    cli::{Chat, History},
    client::{read, submit},
    error, unsupported,
};
use aq_human_http::Client;
use serde_json::Value;
use uuid::Uuid;

async fn discovery(http: &Client) -> Result<(Uuid, String)> {
    let value = read(http, "/api/human/conversations/global", &[]).await?;
    if value["contract"] != "human-supervisor-conversation-1" {
        return Err(unsupported("supervisor conversation contract"));
    }
    let id = value["conversation"]["id"]
        .as_str()
        .and_then(|v| v.parse::<Uuid>().ok())
        .filter(|id| !id.is_nil())
        .ok_or_else(|| {
            error(
                "invalid_response",
                "Invalid supervisor conversation identity",
            )
        })?;
    let revision = value["conversation"]["revision"].as_str().ok_or_else(|| {
        error(
            "invalid_response",
            "Invalid supervisor conversation revision",
        )
    })?;
    Ok((id, revision.into()))
}
pub async fn history(http: &Client, args: &History) -> Result<Value> {
    let (id, _) = discovery(http).await?;
    let mut query = vec![];
    if let Some(limit) = args.limit {
        query.push(("limit".into(), limit.to_string()));
    }
    if let Some(before) = &args.before {
        query.push(("before".into(), before.clone()));
    }
    if let Some(after) = &args.after {
        query.push(("after".into(), after.clone()));
    }
    read(
        http,
        &format!("/api/human/conversations/{id}/messages"),
        &query,
    )
    .await
}
pub async fn send(http: &Client, text: &str) -> Result<Value> {
    let (id, revision) = discovery(http).await?;
    submit(http, &actions::message(id, revision, text)).await
}
pub async fn chat(http: &Client, args: &Chat) -> Result<Value> {
    if args.once.is_none() {
        return history(http, &args.history).await;
    }
    // Version 1 deliberately has only human messages and no runtime replies.
    // A send receipt proves queue admission, never a supervisor answer. F1b owns
    // publishing the live reply/correlation contract; never guess its fields.
    let _ = discovery(http).await?;
    Err(unsupported(
        "live supervisor reply (chat --once); requires F1b",
    ))
}

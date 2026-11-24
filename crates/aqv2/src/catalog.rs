//! The published command contract (`protocol/commands/v1/commands.json`), as
//! built into this binary. It is a hint, never the authority: it supplies
//! command kinds, param schemas and CLI tokens for help and for filling in
//! operation IDs. A command the server registers after this binary was built
//! is still reachable through `run` (and its CLI tokens through discovery);
//! the server decides whether the caller may run it.
use serde_json::Value;
use std::sync::OnceLock;

const COMMANDS: &str = include_str!("../../../protocol/commands/v1/commands.json");

fn document() -> &'static Value {
    static DOC: OnceLock<Value> = OnceLock::new();
    DOC.get_or_init(|| serde_json::from_str(COMMANDS).expect("built-in commands.json"))
}
pub fn commands() -> &'static [Value] {
    document()["commands"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
}
pub fn entry(name: &str) -> Option<&'static Value> {
    commands().iter().find(|c| c["name"] == name)
}
/// `Some(true)` for a known mutation, `Some(false)` for a known read.
pub fn is_mutation(name: &str) -> Option<bool> {
    entry(name).map(|c| c["kind"] == "mutation")
}
/// Whether the principal's params for `name` carry the operation ID: an inline
/// schema with an `operation_id` property, an action-union payload, or an
/// agent-access mailbox mutation.
pub fn takes_operation_id(name: &str, principal: &str) -> bool {
    let Some(route) = entry(name).map(|c| &c["principals"][principal]) else {
        return false;
    };
    if route["body"] == "action_request" {
        return true;
    }
    let schema = &route["params_schema"];
    if schema["properties"].get("operation_id").is_some() {
        return true;
    }
    schema["$ref"].as_str().is_some_and(|r| {
        r.starts_with("protocol/dashboard-actions/v1/schema.json#kind=")
            || r.ends_with("/$defs/MessageSend")
            || r.ends_with("/$defs/MessageAck")
    })
}
/// The command whose CLI tokens are the longest prefix of `tokens`, with the
/// number of tokens it used. `table` is the catalog or a discovery listing.
pub fn resolve<'a>(
    table: impl IntoIterator<Item = &'a Value>,
    tokens: &[String],
) -> Option<(String, usize)> {
    let mut best: Option<(String, usize)> = None;
    for c in table {
        let Some(cli) = c["cli"].as_array() else {
            continue;
        };
        let n = cli.len();
        let minimum = best.as_ref().map_or(1, |(_, previous)| previous + 1);
        if !(minimum..=tokens.len()).contains(&n) {
            continue;
        }
        if cli.iter().zip(tokens).all(|(a, b)| a == b.as_str())
            && let Some(name) = c["name"].as_str()
        {
            best = Some((name.to_owned(), n));
        }
    }
    best
}

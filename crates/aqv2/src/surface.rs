//! The generic command surface (spec 0006 §4.3): `POST /api/v1/commands/{name}`
//! and `GET /api/v1/commands`. Every command, typed or not, goes through
//! [`invoke`], so output, exit codes, operation IDs and recovery hints come
//! from one place in both modes.
use crate::{
    Ctx, Mode, Result, catalog, cli::RunFlags, client, error, flags, input, program,
    with_operation_in,
};
use clap::Parser;
use serde_json::{Map, Value, json};
use uuid::Uuid;

/// One `CommandRequest` for one command.
#[derive(Debug, Clone, PartialEq)]
pub struct Invocation {
    pub name: String,
    pub request: Map<String, Value>,
    /// Set for a mutation (or a command this binary does not know): it is
    /// reported with any uncertain outcome so the caller can inspect it.
    pub operation_id: Option<Uuid>,
}
impl Invocation {
    /// A read: only `params`.
    pub fn read(name: &str, params: Map<String, Value>) -> Self {
        let mut request = Map::new();
        request.insert("params".into(), Value::Object(params));
        Self {
            name: name.into(),
            request,
            operation_id: None,
        }
    }
    /// A mutation with a fresh operation ID; `in_params` also binds it into
    /// `params` (the route's own body carries it).
    pub fn mutation(name: &str, mut params: Map<String, Value>, in_params: bool) -> Self {
        let id = Uuid::new_v4();
        if in_params {
            params.insert("operation_id".into(), json!(id));
        }
        let mut request = Map::new();
        request.insert("operation_id".into(), json!(id));
        request.insert("params".into(), Value::Object(params));
        Self {
            name: name.into(),
            request,
            operation_id: Some(id),
        }
    }
    /// An action-union subject; its revision becomes the expected version.
    pub fn subject(mut self, kind: &str, id: impl ToString, revision: Option<&str>) -> Self {
        let mut subject = json!({"type":kind,"id":id.to_string()});
        let mut expected = Map::new();
        if let Some(r) = revision {
            subject["revision"] = json!(r);
            expected.insert(kind.into(), json!(r));
        }
        self.request.insert("subject".into(), subject);
        self.request
            .insert("expected_versions".into(), Value::Object(expected));
        self
    }
    pub fn reason(mut self, reason: &str) -> Self {
        self.request.insert("reason".into(), json!(reason));
        self
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        && !name.starts_with('.')
}

/// The structured refusal (§6.8), keeping only its published fields.
pub fn refusal(body: &Value) -> Option<Value> {
    let code = body["code"].as_str()?;
    let known = crate::REFUSED.contains(&code)
        || matches!(
            code,
            "unknown_command"
                | "invalid_request"
                | "internal"
                | "overloaded"
                | "unsupported_api_version"
        );
    let message = body["message"]
        .as_str()
        .filter(|m| !m.is_empty() && m.len() <= 4096)?;
    if !known || body["effect"] != "not_applied" || !body["command"].is_string() {
        return None;
    }
    let held = body["held"]
        .as_array()
        .map(|h| h.iter().take(64).cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let mut r = json!({"code":code,"command":body["command"],"grant":body["grant"],"held":held,
        "message":message,"effect":"not_applied","request_id":body["request_id"]});
    if body["retryable"].is_boolean() {
        r["retryable"] = body["retryable"].clone();
    }
    Some(r)
}
/// The disposition of a 2xx mutation response that did not take effect yet:
/// a dashboard-actions envelope that is not `confirmed`, or an agent-access
/// operation that is pending, unknown, conflicting or rejected.
fn unconfirmed(body: &Value) -> Option<&str> {
    if body["api_version"] == "dashboard-actions-1" {
        return body["data"]["disposition"]
            .as_str()
            .filter(|d| *d != "confirmed");
    }
    if body["operation_id"].is_string() && body["payload_digest"].is_string() {
        return body["state"]
            .as_str()
            .filter(|s| matches!(*s, "pending" | "unknown" | "conflict" | "rejected"));
    }
    None
}

/// Send one invocation and classify the response. A mutation is sent once,
/// never resent; any uncertain outcome carries its operation ID.
pub async fn invoke(ctx: &Ctx, inv: &Invocation) -> Result<Value> {
    if !valid_name(&inv.name) {
        return Err(error(
            "invalid_arguments",
            "A command name is lowercase letters, digits, `.`, `_` and `-`",
        ));
    }
    let attach = |e: Value| match inv.operation_id {
        Some(id) => with_operation_in(ctx.mode, e, id),
        None => e,
    };
    let raw = ctx
        .http
        .post_json_response(
            &format!("/api/v1/commands/{}", inv.name),
            &Value::Object(inv.request.clone()),
        )
        .await
        .map_err(|e| attach(client::http_error(e)))?;
    if (200..300).contains(&raw.status) {
        if let Some(id) = inv.operation_id {
            if raw.body["api_version"] == "dashboard-actions-1" {
                client::validate_receipt(&raw.body, id).map_err(attach)?;
            }
            if unconfirmed(&raw.body).is_some() {
                let mut e = attach(error(
                    "operation_unconfirmed",
                    "Operation is not confirmed; inspect its original ID before taking further action",
                ));
                e["operation"] = raw.body;
                return Err(e);
            }
        }
        return Ok(raw.body);
    }
    if let Some(mut r) = refusal(&raw.body) {
        // A refusal is `not_applied`: nothing to recover, but keep the ID.
        if let Some(id) = inv.operation_id {
            r["operation_id"] = json!(id);
        }
        return Err(r);
    }
    Err(attach(client::response(raw).err().unwrap_or_else(|| {
        error("invalid_response", "Unexpected command response")
    })))
}

/// `run NAME`: params from flags; a mutation gets a fresh operation ID, also
/// bound into params when the command's params carry one.
pub fn build(mode: crate::Mode, name: &str, flags: &RunFlags) -> Result<Invocation> {
    let mut params = input::params(flags.params_file.as_deref(), &flags.params)?;
    let from_params = params
        .get("operation_id")
        .and_then(|v| v.as_str()?.parse::<Uuid>().ok());
    // A command this binary does not know is treated as a possible mutation.
    let mutation = catalog::is_mutation(name).unwrap_or(true);
    let op = flags
        .operation_id
        .or(from_params)
        .or_else(|| mutation.then(Uuid::new_v4));
    if let Some(id) = op
        && catalog::takes_operation_id(name, mode.principal())
    {
        params.entry("operation_id").or_insert(json!(id));
    }
    let mut request = Map::new();
    if let Some(id) = op {
        request.insert("operation_id".into(), json!(id));
    }
    if let Some(s) = &flags.subject {
        let subject = input::subject(s)?;
        let mut expected = input::expected(&flags.expect)?;
        if expected.is_empty()
            && let Some(r) = subject.get("revision")
        {
            expected.insert(
                subject["type"].as_str().unwrap_or_default().into(),
                r.clone(),
            );
        }
        request.insert("subject".into(), subject);
        request.insert("expected_versions".into(), Value::Object(expected));
    } else if !flags.expect.is_empty() {
        request.insert(
            "expected_versions".into(),
            Value::Object(input::expected(&flags.expect)?),
        );
    }
    if let Some(r) = &flags.reason {
        request.insert("reason".into(), json!(r));
    }
    request.insert("params".into(), Value::Object(params));
    Ok(Invocation {
        name: name.into(),
        request,
        operation_id: op,
    })
}
pub async fn run(ctx: &Ctx, name: &str, flags: &RunFlags) -> Result<Value> {
    if flags.describe {
        return describe(name);
    }
    invoke(ctx, &build(ctx.mode, name, flags)?).await
}
/// The built-in catalog entry: summary, grant, target and params per principal.
pub fn describe(name: &str) -> Result<Value> {
    catalog::entry(name).cloned().ok_or_else(|| {
        error(
            "unknown_command",
            &format!(
                "{name} is not in this binary's catalog; `{} commands --all` asks the server",
                program()
            ),
        )
    })
}

/// `GET /api/v1/commands[?all=true]`.
pub async fn discover(ctx: &Ctx, all: bool) -> Result<Value> {
    let query = if all {
        vec![("all".to_owned(), "true".to_owned())]
    } else {
        vec![]
    };
    let query: Vec<_> = query
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let raw = ctx
        .http
        .get_json_response("/api/v1/commands", &query)
        .await
        .map_err(client::http_error)?;
    if (200..300).contains(&raw.status) {
        if raw.body["api_version"] != 1 || !raw.body["commands"].is_array() {
            return Err(error("invalid_response", "Invalid command discovery"));
        }
        return Ok(raw.body);
    }
    if let Some(r) = refusal(&raw.body) {
        return Err(r);
    }
    client::response(raw)
}
pub async fn whoami(ctx: &Ctx) -> Result<Value> {
    let value = invoke(ctx, &Invocation::read("whoami", Map::new())).await?;
    if value["api_version"] != 1 || !value["principal"].is_string() {
        return Err(error("invalid_response", "Invalid whoami response"));
    }
    Ok(value)
}

/// Flags after a command's CLI tokens: the same as `run`.
#[derive(Parser)]
struct ExternalFlags {
    #[command(flatten)]
    flags: RunFlags,
    #[arg(long)]
    json: bool,
}
/// `aq <cli tokens…> [run flags]`: resolve the tokens against the built-in
/// catalog, then the server's discovery (so a newer command's tokens work too).
pub async fn external(ctx: &Ctx, tokens: &[String]) -> Result<Value> {
    if let Some(help) = help(ctx.mode, tokens) {
        return Ok(help);
    }
    let words: Vec<String> = tokens
        .iter()
        .take_while(|t| !t.starts_with('-'))
        .cloned()
        .collect();
    let found = match catalog::resolve(catalog::commands(), &words) {
        Some(found) => Some(found),
        None => {
            let listing = discover(ctx, true).await?;
            let table = listing["commands"]
                .as_array()
                .into_iter()
                .chain(listing["unavailable"].as_array())
                .flatten();
            catalog::resolve(table, &words)
        }
    };
    let Some((name, used)) = found else {
        return Err(error(
            "unknown_command",
            &format!(
                "No command `{}`; `{} commands --all` lists them",
                words.join(" "),
                program()
            ),
        ));
    };
    // The command's own params as flags (flags.rs), then `run`'s flags. A
    // missing required param is the server's to refuse: the catalog is a hint.
    let generated = flags::flags(&name, ctx.mode.principal());
    let (params, rest) = flags::split(&generated, &tokens[used..])?;
    let usage = format!(
        "`{} {} --help` lists its flags",
        program(),
        words[..used].join(" ")
    );
    let parsed = ExternalFlags::try_parse_from(
        std::iter::once(program()).chain(rest.iter().map(String::as_str)),
    )
    .map_err(|_| {
        error(
            "invalid_arguments",
            &format!("Invalid arguments for {name}; {usage}"),
        )
    })?;
    if parsed.flags.describe {
        return describe(&name);
    }
    let mut inv = build(ctx.mode, &name, &parsed.flags)?;
    if let Some(Value::Object(all)) = inv.request.get_mut("params") {
        for (key, value) in params {
            if all.insert(key.clone(), value).is_some() {
                return Err(error(
                    "invalid_arguments",
                    &format!("{key} is given both as a flag and as a --param"),
                ));
            }
        }
    }
    invoke(ctx, &inv).await
}
/// Help that needs no connection: `<command tokens> --help` (or `-h`) for a
/// command in the built-in catalog, and the commands under tokens that name
/// a group but no command. `{"help": text}`, or `None` to run the command.
pub fn help(mode: Mode, tokens: &[String]) -> Option<Value> {
    let args: Vec<&String> = tokens.iter().take_while(|t| *t != "--").collect();
    let words: Vec<String> = args
        .iter()
        .take_while(|t| !t.starts_with('-'))
        .map(|t| (*t).clone())
        .collect();
    let asked = args.iter().any(|t| *t == "--help" || *t == "-h");
    let text = match catalog::resolve(catalog::commands(), &words) {
        Some((name, _)) if asked => flags::help(&name, mode.principal()),
        Some(_) => None,
        None => flags::group_help(&words),
    };
    text.map(|text| json!({"help":text}))
}

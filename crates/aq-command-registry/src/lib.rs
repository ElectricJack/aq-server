//! The AQ command registry (spec 0006 §4). Every command, read or mutation,
//! is declared once here or by its `ActionKind`: its name, the grant it needs,
//! how its scope target is derived, and how each principal reaches it.
//!
//! Dispatch is route-agnostic. A command names the existing route that serves
//! it for each principal (the owner's REST read or the action union, the
//! supervisor's machine action route, the agent's `/v1/agent/*` route), and
//! `POST /api/v1/commands/{name}` builds that request from the command's
//! `params` and forwards it in process. So a command runs the same handler,
//! receipts and idempotency as its REST form, and adding a command needs no
//! change to the CLI, the channel proxy or the worker.
//!
//! This crate is pure: aq-server authenticates the caller, resolves targets,
//! forwards requests and records audit rows.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

pub mod agent_task;
mod catalog;
mod queries;
pub mod routes;
mod subjects;
mod summaries;
pub use catalog::catalog;
pub use queries::QueryParam;
pub use subjects::{Id as SubjectId, SubjectRule, subject as subject_rule};
pub use summaries::{PLACEHOLDER_SUMMARY, action_summary};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Read,
    Mutation,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Principal {
    Owner,
    Supervisor,
    Agent,
}
impl Principal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Supervisor => "supervisor",
            Self::Agent => "agent",
        }
    }
}

/// How the scope target is derived from a request (§6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetRule {
    /// The caller's own attempt (its questions, waits, mail, identity).
    Own,
    /// The attempt's own task (its comment thread).
    OwnTask,
    /// The task named by this param.
    Task(&'static str),
    /// A create under the task named by `field`; with no parent the command
    /// needs `unparented` (a project-level create) instead.
    ParentTask {
        field: &'static str,
        unparented: &'static str,
    },
    /// The project named by this param (the binding's project when omitted).
    Project(&'static str),
    /// The request `subject` (a task, comment or knowledge record).
    Subject,
    /// The mailbox named by this param: a task mailbox is that task; the
    /// caller's own session and the supervisor are its own attempt.
    Mailbox(&'static str),
    /// The comment named by this param: the task it is on, related to the
    /// attempt's task, when the caller wrote it; any other comment is
    /// outside every scope (an agent edits and deletes only its own).
    Comment(&'static str),
}
impl TargetRule {
    pub fn describe(&self) -> Value {
        match self {
            Self::Own => json!({"rule":"own"}),
            Self::OwnTask => json!({"rule":"own_task"}),
            Self::Task(f) => json!({"rule":"task","param":f}),
            Self::ParentTask { field, unparented } => {
                json!({"rule":"parent_task","param":field,"unparented_grant":unparented})
            }
            Self::Project(f) => json!({"rule":"project","param":f}),
            Self::Subject => json!({"rule":"subject"}),
            Self::Mailbox(f) => json!({"rule":"mailbox","param":f}),
            Self::Comment(f) => json!({"rule":"comment","param":f}),
        }
    }
}

/// How a command's `params` become the body of the route that serves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    /// The body is `params`.
    Params,
    /// The body is `params` plus `"api_version":1`.
    Versioned,
    /// A GET: path segments come from `params`, every other param is a query
    /// string value.
    Query,
    /// An agent-access-1 graph read: `{api_version, request_id, binding,
    /// query}` where `query` is `params` (plus `kind` when given, with the
    /// route's `rename` applied as `(query field, param)`) and every field
    /// of the defaults object (inline JSON) the params omit.
    GraphRead(Option<&'static str>, &'static str),
    /// An agent-access-1 graph command: `params` plus the binding.
    GraphCommand,
    /// The action union: an `ActionRequest` whose payload is `params`.
    Action,
    /// Answered by the command surface itself (`whoami`).
    Local,
}

/// Where a command's params schema comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Params {
    /// The action payload schema the `ActionKind` registers.
    Action,
    /// Generated from the route's path segments and the query params
    /// declared for its path ([`Route::query`]); any other param is refused.
    Path,
    /// A published schema elsewhere in the repository.
    Ref(&'static str),
    /// An inline JSON Schema.
    Inline(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub method: &'static str,
    pub path: &'static str,
    pub body: Body,
    pub params: Params,
    /// `{segment}` → param name, for segments whose param is named differently.
    pub rename: &'static [(&'static str, &'static str)],
}
impl Route {
    pub const fn action() -> Self {
        Self {
            method: "POST",
            path: "/api/v1/actions",
            body: Body::Action,
            params: Params::Action,
            rename: &[],
        }
    }
    pub const fn get(path: &'static str, rename: &'static [(&'static str, &'static str)]) -> Self {
        Self {
            method: "GET",
            path,
            body: Body::Query,
            params: Params::Path,
            rename,
        }
    }
    pub const fn post(path: &'static str, body: Body, params: Params) -> Self {
        Self {
            method: "POST",
            path,
            body,
            params,
            rename: &[],
        }
    }
    pub const fn local() -> Self {
        Self {
            method: "POST",
            path: "/api/v1/commands/whoami",
            body: Body::Local,
            params: Params::Inline(r#"{"type":"object","additionalProperties":false}"#),
            rename: &[],
        }
    }
    /// The query params the handler at this path accepts, for a GET. `None`
    /// when the path has no declaration (a test requires one per GET).
    pub fn query(&self) -> Option<&'static [QueryParam]> {
        queries::declared(self.path)
    }
    /// Whether the route's inline params schema has this property.
    fn takes(&self, field: &str) -> bool {
        let Params::Inline(schema) = self.params else {
            return false;
        };
        serde_json::from_str::<Value>(schema).is_ok_and(|s| s["properties"].get(field).is_some())
    }
    fn segments(&self) -> impl Iterator<Item = (&'static str, &'static str)> + '_ {
        self.path.split('/').filter_map(|s| {
            let seg = s.strip_prefix('{')?.strip_suffix('}')?;
            let param = self
                .rename
                .iter()
                .find(|(from, _)| *from == seg)
                .map_or(seg, |(_, to)| *to);
            Some((seg, param))
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub kind: Kind,
    /// The operation the caller must hold. `None` only for `whoami` and
    /// operation lookups, which any authenticated caller may run on itself.
    /// A grant outside the agent vocabulary makes the command owner-only.
    pub grant: Option<&'static str>,
    pub target: TargetRule,
    pub summary: &'static str,
    pub owner: Option<Route>,
    pub supervisor: Option<Route>,
    pub agent: Option<Route>,
}
impl CommandSpec {
    pub fn route(&self, p: Principal) -> Option<&Route> {
        match p {
            Principal::Owner => self.owner.as_ref(),
            Principal::Supervisor => self.supervisor.as_ref(),
            Principal::Agent => self.agent.as_ref(),
        }
    }
    pub fn owner_only(&self) -> bool {
        self.grant
            .is_some_and(|g| !aq_agent_grants::grantable(g) || aq_agent_grants::owner_only(g))
    }
    /// The CLI tokens: `aq task comment add` for `task.comment.add`.
    pub fn cli(&self) -> Vec<String> {
        self.name.split('.').map(|s| s.replace('_', "-")).collect()
    }
}

/// A mutation as an `ActionKind` declares it: its kind, grant and target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionSpec {
    pub kind: &'static str,
    pub grant: &'static str,
    pub target: TargetRule,
}

/// The registry: the static catalog merged with the registered action kinds.
#[derive(Debug, Clone)]
pub struct Registry {
    commands: Vec<CommandSpec>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryError(pub String);
impl Registry {
    /// Every action kind becomes a mutation command reached by the owner
    /// through the action union. A catalog entry with the same name adds
    /// the other principals' routes and must agree on the grant.
    pub fn new(actions: &[ActionSpec]) -> Result<Self, RegistryError> {
        let mut commands = catalog();
        for a in actions {
            match commands.iter_mut().find(|c| c.name == a.kind) {
                Some(c) => {
                    if c.grant != Some(a.grant) || c.kind != Kind::Mutation || c.owner.is_some() {
                        return Err(RegistryError(format!(
                            "{}: catalog and ActionKind disagree",
                            a.kind
                        )));
                    }
                    // The catalog's target is the agent-facing one (an agent
                    // comments on its own task); the owner's action derives
                    // its own subject.
                    c.owner = Some(Route::action());
                }
                None => commands.push(CommandSpec {
                    name: a.kind,
                    kind: Kind::Mutation,
                    grant: Some(a.grant),
                    target: a.target,
                    summary: action_summary(a.kind).unwrap_or(PLACEHOLDER_SUMMARY),
                    owner: Some(Route::action()),
                    supervisor: None,
                    agent: None,
                }),
            }
        }
        for command in &mut commands {
            if matches!(
                command.name,
                "task.edit"
                    | "task.reprioritize"
                    | "task.dependency.add"
                    | "task.dependency.remove"
            ) {
                command.supervisor = Some(Route::action());
            }
        }
        commands.sort_by(|a, b| a.name.cmp(b.name));
        for pair in commands.windows(2) {
            if pair[0].name == pair[1].name {
                return Err(RegistryError(format!("duplicate {}", pair[0].name)));
            }
        }
        for c in &commands {
            if !valid_name(c.name) {
                return Err(RegistryError(format!("invalid name {}", c.name)));
            }
            let routes = [&c.owner, &c.supervisor, &c.agent];
            if routes.iter().all(|r| r.is_none()) {
                return Err(RegistryError(format!("{} has no route", c.name)));
            }
            if routes
                .iter()
                .copied()
                .flatten()
                .any(|r| r.params == Params::Action)
                && !actions.iter().any(|a| a.kind == c.name)
            {
                return Err(RegistryError(format!(
                    "{} is not a registered action",
                    c.name
                )));
            }
        }
        Ok(Self { commands })
    }
    pub fn commands(&self) -> &[CommandSpec] {
        &self.commands
    }
    pub fn get(&self, name: &str) -> Option<&CommandSpec> {
        self.commands.iter().find(|c| c.name == name)
    }
}
pub fn valid_name(name: &str) -> bool {
    name.len() <= 128
        && name.split('.').all(|part| {
            part.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

/// `POST /api/v1/commands/{name}` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CommandRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<Uuid>,
    /// `{type,id,revision?}` for an action whose subject is not derivable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_versions: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default)]
    pub params: Map<String, Value>,
}

/// A request the command surface forwards to the route that serves it.
#[derive(Debug, Clone, PartialEq)]
pub struct Forward {
    pub method: &'static str,
    pub uri: String,
    pub body: Option<Value>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildError(pub &'static str);

/// Build the forwarded request. `binding` is the agent-access-1 binding for
/// an agent caller (graph reads and commands carry it).
pub fn forward(
    spec: &CommandSpec,
    route: &Route,
    request: &CommandRequest,
    binding: Option<&Value>,
    request_id: Uuid,
) -> Result<Forward, BuildError> {
    let mut params = request.params.clone();
    let mut uri = String::new();
    for part in route.path.split('/').filter(|s| !s.is_empty()) {
        uri.push('/');
        match part.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            Some(seg) => {
                let param = route.segments().find(|(s, _)| *s == seg).map(|(_, p)| p);
                let value = params
                    .remove(param.unwrap_or(seg))
                    .ok_or(BuildError("missing path param"))?;
                let text = scalar(&value).ok_or(BuildError("path param must be a scalar"))?;
                if text.is_empty()
                    || !text
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b':')
                {
                    return Err(BuildError("path param is not a plain identifier"));
                }
                uri.push_str(&text);
            }
            None => uri.push_str(part),
        }
    }
    let body = match route.body {
        Body::Local => None,
        Body::Query => {
            let mut first = true;
            let declared = route.query().unwrap_or_default();
            for (k, v) in &params {
                if !declared.iter().any(|q| q.name == k) {
                    return Err(BuildError("unknown param"));
                }
                let text = scalar(v).ok_or(BuildError("query param must be a scalar"))?;
                uri.push(if first { '?' } else { '&' });
                first = false;
                uri.push_str(&encode(k));
                uri.push('=');
                uri.push_str(&encode(&text));
            }
            None
        }
        Body::Params => {
            // A route that takes the operation ID or a reason in its body
            // gets the request's own when the params omit them.
            for (field, value) in [
                ("operation_id", request.operation_id.map(|op| json!(op))),
                ("reason", request.reason.as_ref().map(|r| json!(r))),
            ] {
                if let Some(value) = value
                    && route.takes(field)
                {
                    params.entry(field).or_insert(value);
                }
            }
            Some(Value::Object(params))
        }
        Body::Versioned => {
            params.insert("api_version".into(), json!(1));
            Some(Value::Object(params))
        }
        Body::GraphRead(kind, defaults) => {
            let binding = binding.ok_or(BuildError("graph reads need a binding"))?;
            let mut query = if kind.is_none() {
                match params.remove("query") {
                    Some(Value::Object(q)) if params.is_empty() => q,
                    _ => return Err(BuildError("graph.read takes one `query` object")),
                }
            } else {
                params
            };
            for (field, param) in route.rename {
                if let Some(v) = query.remove(*param) {
                    query.insert((*field).into(), v);
                }
            }
            if let Some(kind) = kind {
                query.insert("kind".into(), json!(kind));
                for (k, v) in graph_defaults(defaults) {
                    query.entry(k).or_insert(v);
                }
            }
            Some(json!({"api_version":1,"request_id":request_id,"binding":binding,"query":query}))
        }
        Body::GraphCommand => {
            let binding = binding.ok_or(BuildError("graph commands need a binding"))?;
            params.insert("api_version".into(), json!(1));
            params.insert("binding".into(), binding.clone());
            if let Some(op) = request.operation_id {
                params.entry("operation_id").or_insert(json!(op));
            }
            if let Some(reason) = &request.reason {
                params.entry("reason").or_insert(json!(reason));
            }
            Some(Value::Object(params))
        }
        Body::Action => {
            let op = request
                .operation_id
                .or_else(|| {
                    params
                        .get("operation_id")
                        .and_then(|v| v.as_str()?.parse().ok())
                })
                .ok_or(BuildError("a mutation needs an operation_id"))?;
            // An explicit subject is sent as given; otherwise it follows
            // from the params by the kind's rule (subjects.rs).
            let (subject, derived) = match &request.subject {
                Some(subject) => (subject.clone(), Map::new()),
                None => derive_subject(spec.name, &mut params)?,
            };
            let expected = request.expected_versions.clone().unwrap_or(derived);
            // Every payload carries the operation ID.
            params.entry("operation_id").or_insert(json!(op));
            let mut action = json!({"operation_id":op,"kind":spec.name,"subject":subject,
                "expected_versions":expected,"payload":params});
            if let Some(reason) = &request.reason {
                action["reason"] = json!(reason);
            }
            Some(action)
        }
    };
    Ok(Forward {
        method: route.method,
        uri,
        body,
    })
}
/// The subject and expected versions of `kind` from its params. A param
/// that only names the subject is taken out of `params` (the payload).
fn derive_subject(
    kind: &str,
    params: &mut Map<String, Value>,
) -> Result<(Value, Map<String, Value>), BuildError> {
    const MISSING: BuildError =
        BuildError("an action needs its subject: the id param its schema names, or `subject`");
    let rule = subjects::subject(kind).ok_or(BuildError("an action needs a subject"))?;
    let id = match rule.id {
        SubjectId::Fixed(id) => id.to_owned(),
        SubjectId::Payload(p) => params.get(p).and_then(scalar).ok_or(MISSING)?,
        SubjectId::Param(p) => params.remove(p).as_ref().and_then(scalar).ok_or(MISSING)?,
    };
    let mut subject = json!({"type":rule.entity,"id":id});
    let mut expected = Map::new();
    if let Some((param, key)) = rule.revision {
        let revision = params
            .get(param)
            .and_then(scalar)
            .ok_or(BuildError("an action needs the version it expects"))?;
        subject["revision"] = json!(revision);
        expected.insert(key.into(), json!(revision));
    }
    Ok((subject, expected))
}
fn graph_defaults(defaults: &str) -> Map<String, Value> {
    serde_json::from_str(defaults).unwrap_or_default()
}
fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The params schema a principal's route takes.
pub fn params_schema(
    route: &Route,
    action_schemas: &std::collections::BTreeMap<String, Value>,
    name: &str,
) -> Value {
    match route.params {
        Params::Action => action_schemas.get(name).cloned().unwrap_or(Value::Null),
        Params::Ref(r) => json!({"$ref": r}),
        Params::Inline(s) => serde_json::from_str(s).expect("static inline schema"),
        Params::Path => {
            let mut props = Map::new();
            let mut required = Vec::new();
            for (_, param) in route.segments() {
                props.insert(
                    param.into(),
                    json!({"type":"string","minLength":1,"maxLength":128}),
                );
                required.push(json!(param));
            }
            for q in route.query().unwrap_or_default() {
                let schema = serde_json::from_str(q.schema).expect("static query schema");
                props.insert(q.name.into(), schema);
                if q.required {
                    required.push(json!(q.name));
                }
            }
            json!({"type":"object","properties":props,"required":required,
                "additionalProperties":false,
                "description":"Path params as named; the other params are the route's query string. A param not listed is refused."})
        }
    }
}
pub fn digest(value: &Value) -> String {
    use sha2::{Digest, Sha256};
    let mut v = value.clone();
    v.sort_all_objects();
    format!(
        "sha256:{}",
        hex::encode(Sha256::digest(serde_json::to_vec(&v).expect("json")))
    )
}

/// `protocol/commands/v1/commands.json`: generated from the registry.
pub fn document(
    registry: &Registry,
    action_schemas: &std::collections::BTreeMap<String, Value>,
) -> Value {
    let commands: Vec<Value> = registry
        .commands()
        .iter()
        .map(|c| {
            let mut principals = Map::new();
            for p in [Principal::Owner, Principal::Supervisor, Principal::Agent] {
                let Some(r) = c.route(p) else { continue };
                let schema = params_schema(r, action_schemas, c.name);
                let mut entry = json!({"route":{"method":r.method,"path":r.path},
                    "body":body_name(r.body),"params_digest":digest(&schema)});
                // A graph read's query kind, the defaults for omitted fields
                // and the params sent under another query field name.
                if let Body::GraphRead(Some(kind), defaults) = r.body {
                    let renamed: Map<String, Value> =
                        r.rename.iter().map(|(f, p)| ((*p).into(), json!(f))).collect();
                    entry["graph_query"] = json!({"kind":kind,"defaults":graph_defaults(defaults),"rename":renamed});
                }
                // How the action's subject follows from the params.
                if r.body == Body::Action
                    && let Some(rule) = subjects::subject(c.name)
                {
                    entry["subject"] = rule.describe();
                }
                if matches!(r.params, Params::Ref(_) | Params::Inline(_) | Params::Path) {
                    entry["params_schema"] = schema;
                } else {
                    entry["params_schema"] = json!({"$ref":format!("protocol/dashboard-actions/v1/schema.json#kind={}", c.name)});
                }
                principals.insert(p.as_str().into(), entry);
            }
            json!({"name":c.name,"kind":c.kind,"grant":c.grant,"owner_only":c.owner_only(),
                "target":c.target.describe(),"summary":c.summary,"cli":c.cli(),"principals":principals})
        })
        .collect();
    json!({
        "version": 1,
        "contract": "commands-1",
        "generated_from": "aq-server command registry (crates/aq-command-registry and the registered ActionKinds); regenerate with `AQ_REGENERATE_COMMANDS=1 cargo nextest run -p aq-server --test it_server -E 'test(/^commands::/)'`",
        "invoke": {"method":"POST","path":"/api/v1/commands/{name}","request":"#/$defs/CommandRequest in schema.json"},
        "discover": {"method":"GET","path":"/api/v1/commands","query":{"all":"true also lists commands the caller cannot run, each with its missing grant"}},
        "listeners": {"admin":"owner (Cloudflare Access)","machine":"agent (per-attempt aqa_ bearer, or the sandbox channel) and supervisor (F1b bearer)"},
        "commands": commands
    })
}
fn body_name(b: Body) -> &'static str {
    match b {
        Body::Params => "params",
        Body::Versioned => "params_with_api_version",
        Body::Query => "query",
        Body::GraphRead(..) => "graph_read",
        Body::GraphCommand => "graph_command",
        Body::Action => "action_request",
        Body::Local => "local",
    }
}

#[cfg(test)]
mod tests;

/// The agent's scope target for `rule` (§6.4). Params gain defaults from
/// the binding: an omitted `project_id` is the attempt's project and is
/// removed before forwarding (agent routes take it from the binding), an
/// omitted task param is the attempt's task, and a task equal to the
/// attempt's task is `Own`. `relation` is the server's resolution of any
/// other named task (its ancestry), or of a `Comment` rule's comment.
pub fn agent_target(
    rule: TargetRule,
    params: &mut Map<String, Value>,
    holder: aq_agent_grants::Holder,
    own_task: Uuid,
    relation: aq_agent_grants::Relation,
) -> Result<aq_agent_grants::Target, BuildError> {
    use aq_agent_grants::{Relation, Target};
    let id = |v: &Value| v.as_str().and_then(|s| s.parse::<Uuid>().ok());
    let task = |t: Uuid| Target::Task {
        project: holder.project,
        relation: if t == own_task {
            Relation::Own
        } else {
            relation
        },
    };
    let project = |params: &mut Map<String, Value>, field: &str| match params.remove(field) {
        None => Ok(holder.project),
        Some(v) => id(&v).ok_or(BuildError("project_id must be a UUID")),
    };
    Ok(match rule {
        TargetRule::Own => Target::OwnAttempt,
        TargetRule::OwnTask => task(own_task),
        TargetRule::Project(field) => Target::Project(project(params, field)?),
        TargetRule::Task(field) => {
            let p = project(params, "project_id")?;
            let t = match params.get(field) {
                None => own_task,
                Some(v) => id(v).ok_or(BuildError("task id must be a UUID"))?,
            };
            params.insert(field.into(), json!(t));
            if p != holder.project {
                Target::Project(p)
            } else {
                task(t)
            }
        }
        TargetRule::Comment(field) => {
            params
                .get(field)
                .and_then(id)
                .ok_or(BuildError("comment id must be a UUID"))?;
            Target::Task {
                project: holder.project,
                relation,
            }
        }
        TargetRule::ParentTask { field, .. } => match params.get(field) {
            Some(Value::Null) | None => Target::Project(holder.project),
            Some(v) => {
                let parent = if v == "self" { Some(own_task) } else { id(v) };
                let parent = parent.ok_or(BuildError("parent_id must be a UUID or \"self\""))?;
                // A child of `parent` sits one level below it.
                match task(parent) {
                    Target::Task {
                        project,
                        relation: Relation::Own,
                    } => Target::Task {
                        project,
                        relation: Relation::Descendant(1),
                    },
                    Target::Task {
                        project,
                        relation: Relation::Descendant(d),
                    } => Target::Task {
                        project,
                        relation: Relation::Descendant(d + 1),
                    },
                    other => other,
                }
            }
        },
        TargetRule::Mailbox(field) => match params.get(field) {
            Some(r) if r["kind"] == "task" => {
                let t = id(&r["task_id"]).ok_or(BuildError("task mailbox needs a task_id"))?;
                if r["project_id"] != json!(holder.project) {
                    Target::Project(id(&r["project_id"]).unwrap_or_default())
                } else {
                    task(t)
                }
            }
            Some(_) => Target::OwnAttempt,
            None => return Err(BuildError("a mailbox command needs a recipient")),
        },
        TargetRule::Subject => return Err(BuildError("this command takes no agent subject")),
    })
}
/// The grant a `ParentTask` command needs for its target: the subtask grant
/// under a parent, the project grant without one.
pub fn required_grant(
    spec: &CommandSpec,
    target: &aq_agent_grants::Target,
) -> Option<&'static str> {
    match (spec.target, target) {
        (TargetRule::ParentTask { unparented, .. }, aq_agent_grants::Target::Project(_)) => {
            Some(unparented)
        }
        _ => spec.grant,
    }
}
/// The task id a rule's param names, for the server to resolve its
/// relation to the attempt's task before `agent_target`.
pub fn target_task(rule: TargetRule, params: &Map<String, Value>) -> Option<Uuid> {
    let v = match rule {
        TargetRule::Task(f) | TargetRule::ParentTask { field: f, .. } => params.get(f)?,
        TargetRule::Mailbox(f) => params.get(f)?.get("task_id")?,
        _ => return None,
    };
    v.as_str()?.parse().ok()
}
/// The comment id a `Comment` rule names, for the server to resolve.
pub fn target_comment(rule: TargetRule, params: &Map<String, Value>) -> Option<Uuid> {
    match rule {
        TargetRule::Comment(f) => params.get(f)?.as_str()?.parse().ok(),
        _ => None,
    }
}
/// A grant's limits applied to its target: the depth of a created task.
/// `Err(limit name)` when the target is deeper than the grant allows.
pub fn within_limits(
    grant: &aq_agent_grants::Grant,
    target: &aq_agent_grants::Target,
) -> Result<(), &'static str> {
    match (grant.limits.max_depth, target) {
        (
            Some(max),
            aq_agent_grants::Target::Task {
                relation: aq_agent_grants::Relation::Descendant(d),
                ..
            },
        ) if *d > max => Err("max_depth"),
        _ => Ok(()),
    }
}

/// The grant and scope check for one agent command (§6.4), with its
/// target: the target from the params, the grant it needs, the scope and
/// the grant's limits. `relation` is the server's resolution of the named
/// task or comment ([`target_task`], [`target_comment`]).
pub fn authorize(
    spec: &CommandSpec,
    params: &mut Map<String, Value>,
    grants: &aq_agent_grants::GrantSet,
    holder: aq_agent_grants::Holder,
    own_task: Uuid,
    relation: aq_agent_grants::Relation,
) -> (aq_agent_grants::Decision, aq_agent_grants::Target) {
    use aq_agent_grants::{Decision, Grant, Refusal, RefusalCode, Scope, Target};
    let Some(grant) = spec.grant else {
        let own = Grant::new("self", Scope::AttemptSelf);
        return (Decision::Allow(own), Target::OwnAttempt);
    };
    let Ok(target) = agent_target(spec.target, params, holder, own_task, relation) else {
        let r = Refusal::error(
            RefusalCode::InvalidRequest,
            spec.name,
            "A target param is malformed.",
        );
        return (Decision::Deny(r), Target::OwnAttempt);
    };
    let grant = required_grant(spec, &target).unwrap_or(grant);
    match aq_agent_grants::check(grants, holder, spec.name, grant, target) {
        Decision::Allow(g) => match within_limits(&g, &target) {
            Ok(()) => (Decision::Allow(g), target),
            Err(limit) => {
                let held = grants.related(grant);
                let r = Refusal::quota_exceeded(spec.name, grant, held, limit);
                (Decision::Deny(r), target)
            }
        },
        deny => (deny, target),
    }
}
/// Whether a caller may run `spec` at all (discovery, "published means
/// runnable"): `Ok` with the scope of the grant an agent holds for it.
/// `principal` is `None` for a caller with no command principal (a viewer);
/// `grants` is `Some` for an agent.
pub fn available(
    spec: &CommandSpec,
    principal: Option<Principal>,
    grants: Option<&aq_agent_grants::GrantSet>,
) -> Result<Option<String>, aq_agent_grants::RefusalCode> {
    use aq_agent_grants::RefusalCode;
    let p = principal.ok_or(RefusalCode::NotAvailable)?;
    spec.route(p).ok_or(RefusalCode::NotAvailable)?;
    let (Some(grants), Some(grant)) = (grants, spec.grant) else {
        return Ok(None);
    };
    if spec.owner_only() {
        return Err(RefusalCode::OwnerOnly);
    }
    let g = grants.get(grant).ok_or(RefusalCode::GrantMissing)?;
    Ok(Some(g.scope.to_string()))
}
impl Registry {
    /// `GET /api/v1/commands[?all=true]` (a `Discovery`): what the caller may
    /// run, each with the scope of the grant an agent holds, filtered by the
    /// check `invoke` applies ("published means runnable"). `all` also lists
    /// the others with the reason. A caller with no principal is a viewer.
    pub fn discovery(
        &self,
        principal: Option<Principal>,
        grants: Option<&aq_agent_grants::GrantSet>,
        all: bool,
    ) -> Value {
        let (mut runnable, mut unavailable) = (Vec::new(), Vec::new());
        for spec in &self.commands {
            let mut e = json!({"name":spec.name,"kind":spec.kind,"summary":spec.summary,"cli":spec.cli(),"grant":spec.grant});
            match available(spec, principal, grants) {
                Ok(scope) => {
                    e["scope"] = json!(scope);
                    runnable.push(e);
                }
                Err(code) if all => {
                    e["reason"] = json!(code.as_str());
                    unavailable.push(e);
                }
                Err(_) => {}
            }
        }
        let p = principal.map_or("viewer", Principal::as_str);
        let mut body = json!({"api_version":1,"principal":p,"commands":runnable});
        if all {
            body["unavailable"] = json!(unavailable);
        }
        body
    }
    /// The commands an agent holding `grants` may run (`aq commands`).
    pub fn agent_commands(&self, grants: &aq_agent_grants::GrantSet) -> Vec<String> {
        self.commands
            .iter()
            .filter(|c| available(c, Some(Principal::Agent), Some(grants)).is_ok())
            .map(|c| c.name.to_string())
            .collect()
    }
}
/// Whether an agent's response to `spec` is checked by [`agent_result_ok`].
pub fn checks_agent_result(spec: &CommandSpec) -> bool {
    spec.grant == Some(aq_agent_grants::op::TASK_READ)
        && matches!(
            spec.agent.map(|r| r.body),
            Some(Body::GraphRead(Some(_), _))
        )
}
/// A `task.*` agent read is a scoped graph read of the named node, so it
/// must have read a task: a node of another kind is answered as not found
/// (§6.4), never as a knowledge record under the task grant. `body` is the
/// graph read response; reads of other commands always pass.
pub fn agent_result_ok(spec: &CommandSpec, body: &Value) -> bool {
    if !checks_agent_result(spec) {
        return true;
    }
    let result = &body["result"];
    let items = result["items"].as_array().into_iter().flatten();
    std::iter::once(&result["node"])
        .chain(items.map(|i| &i["node"]))
        .filter(|n| !n.is_null())
        .all(|n| n["kind"] == "task")
}

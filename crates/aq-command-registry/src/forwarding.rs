//! Translate a command request to its registered REST or agent route.
use super::*;

/// Agent-access-1 metadata required by graph routes.
pub struct AgentRequest<'a> {
    pub binding: &'a Value,
    pub request_id: Uuid,
}

impl CommandSpec {
    /// Build the forwarded request. `binding` is the agent-access-1 binding for
    /// an agent caller (graph reads and commands carry it).
    pub fn forward(
        &self,
        route: &Route,
        request: &CommandRequest,
        agent: Option<AgentRequest<'_>>,
    ) -> Result<Forward, BuildError> {
        let mut params = request.params.clone();
        let mut uri = route_path(route, &mut params)?;
        let body = match route.body {
            Body::Local => None,
            Body::Query => {
                append_query(&mut uri, route, &params)?;
                None
            }
            Body::Params => Some(params_body(route, request, params)),
            Body::Versioned => {
                params.insert("api_version".into(), json!(1));
                Some(Value::Object(params))
            }
            Body::GraphRead(kind, defaults) => {
                let AgentRequest {
                    binding,
                    request_id,
                } = agent.ok_or(BuildError("graph reads need a binding"))?;
                let query = graph_query(params, route.rename, kind, defaults)?;
                Some(
                    json!({"api_version":1,"request_id":request_id,"binding":binding,"query":query}),
                )
            }
            Body::GraphCommand => {
                let binding = agent
                    .ok_or(BuildError("graph commands need a binding"))?
                    .binding;
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
            Body::Action => Some(action_body(self, request, params)?),
        };
        Ok(Forward {
            method: route.method,
            uri,
            body,
        })
    }
}
fn route_path(route: &Route, params: &mut Map<String, Value>) -> Result<String, BuildError> {
    let mut uri = String::new();
    for part in route.path.split('/').filter(|s| !s.is_empty()) {
        uri.push('/');
        match part.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            Some(seg) => uri.push_str(&path_identifier(route, params, seg)?),
            None => uri.push_str(part),
        }
    }
    Ok(uri)
}
fn path_identifier(
    route: &Route,
    params: &mut Map<String, Value>,
    seg: &str,
) -> Result<String, BuildError> {
    let param = route.segments().find(|(s, _)| *s == seg).map(|(_, p)| p);
    let value = params
        .remove(param.unwrap_or(seg))
        .ok_or(BuildError("missing path param"))?;
    let text = scalar(&value).ok_or(BuildError("path param must be a scalar"))?;
    let plain = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':');
    if text.is_empty() || !text.bytes().all(plain) {
        return Err(BuildError("path param is not a plain identifier"));
    }
    Ok(text)
}
fn append_query(
    uri: &mut String,
    route: &Route,
    params: &Map<String, Value>,
) -> Result<(), BuildError> {
    let declared = route.query().unwrap_or_default();
    for (index, (k, v)) in params.iter().enumerate() {
        if !declared.iter().any(|q| q.name == k) {
            return Err(BuildError("unknown param"));
        }
        let text = scalar(v).ok_or(BuildError("query param must be a scalar"))?;
        uri.push(if index == 0 { '?' } else { '&' });
        uri.push_str(&encode(k));
        uri.push('=');
        uri.push_str(&encode(&text));
    }
    Ok(())
}
fn params_body(route: &Route, request: &CommandRequest, mut params: Map<String, Value>) -> Value {
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
    Value::Object(params)
}
fn graph_query(
    params: Map<String, Value>,
    rename: &[(&str, &str)],
    kind: Option<&str>,
    defaults: &str,
) -> Result<Map<String, Value>, BuildError> {
    let mut query = match kind {
        Some(_) => params,
        None => supplied_query(params)?,
    };
    rename_fields(&mut query, rename);
    if let Some(kind) = kind {
        query.insert("kind".into(), json!(kind));
        for (k, v) in graph_defaults(defaults) {
            query.entry(k).or_insert(v);
        }
    }
    Ok(query)
}
fn rename_fields(query: &mut Map<String, Value>, rename: &[(&str, &str)]) {
    for (field, param) in rename {
        if let Some(v) = query.remove(*param) {
            query.insert((*field).into(), v);
        }
    }
}
fn supplied_query(mut params: Map<String, Value>) -> Result<Map<String, Value>, BuildError> {
    match params.remove("query") {
        Some(Value::Object(query)) if params.is_empty() => Ok(query),
        _ => Err(BuildError("graph.read takes one `query` object")),
    }
}
fn action_body(
    spec: &CommandSpec,
    request: &CommandRequest,
    mut params: Map<String, Value>,
) -> Result<Value, BuildError> {
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
    Ok(action)
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

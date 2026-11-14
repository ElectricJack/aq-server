//! Flags and help generated from a command's published params schema.
//!
//! `aq project show --project-id ID` is `aq run project.show --param
//! project_id=ID`: each property of the schema the built-in catalog publishes
//! for the caller's principal becomes a flag, typed by its schema. The catalog
//! is a hint (see `catalog.rs`): a command or param this binary does not know
//! still goes through `--param`, and the server validates either way.
use crate::{Result, catalog, error, program};
use serde_json::{Map, Value, json};
use std::fmt::Write;
use std::sync::OnceLock;

const ACTIONS: &str = include_str!("../../../protocol/dashboard-actions/v1/schema.json");
const AGENT: &str = include_str!("../../../protocol/agent-access/v1/schema.json");
const ACTIONS_REF: &str = "protocol/dashboard-actions/v1/schema.json#kind=";
const AGENT_REF: &str = "protocol/agent-access/v1/schema.json#/$defs/";

/// Flags of `run` itself; a param with one of these names is given with
/// `--param`. `operation_id` is filled in for a mutation.
const RESERVED: &[&str] = &[
    "param",
    "params_file",
    "subject",
    "expect",
    "reason",
    "operation_id",
    "describe",
    "json",
    "help",
];

fn parsed(text: &'static str, cell: &'static OnceLock<Value>) -> &'static Value {
    cell.get_or_init(|| serde_json::from_str(text).expect("built-in schema"))
}
/// The params schema of `name` for `principal`, with a `$ref` to an action
/// payload or an agent-access definition resolved. `None` when the command or
/// the schema is not in this binary.
pub fn schema(name: &str, principal: &str) -> Option<&'static Value> {
    static ACTIONS_DOC: OnceLock<Value> = OnceLock::new();
    static AGENT_DOC: OnceLock<Value> = OnceLock::new();
    let schema = catalog::entry(name)?["principals"][principal].get("params_schema")?;
    let Some(reference) = schema["$ref"].as_str() else {
        return (schema["type"] == "object").then_some(schema);
    };
    if let Some(kind) = reference.strip_prefix(ACTIONS_REF) {
        return parsed(ACTIONS, &ACTIONS_DOC)["$defs"]["ActionRequest"]["oneOf"]
            .as_array()?
            .iter()
            .find(|k| k["properties"]["kind"]["const"] == kind)
            .map(|k| &k["properties"]["payload"])
            .filter(|p| p["properties"].is_object());
    }
    let def = reference.strip_prefix(AGENT_REF)?;
    parsed(AGENT, &AGENT_DOC)["$defs"]
        .get(def)
        .filter(|p| p["properties"].is_object())
}

/// How a flag's text becomes the param's JSON value.
#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Text,
    Choice(Vec<String>),
    Integer,
    Number,
    Boolean,
    /// Repeatable; each use adds one string.
    TextList,
    /// JSON when the text parses as JSON, else the text itself.
    Any,
}
fn types(schema: &Value) -> Vec<&str> {
    match &schema["type"] {
        Value::String(t) => vec![t.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    }
}
fn choices(schema: &Value) -> Option<Vec<String>> {
    if let Some(c) = schema["const"].as_str() {
        return Some(vec![c.to_owned()]);
    }
    let all = schema["enum"].as_array()?;
    all.iter()
        .map(|v| v.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
}
fn kind(schema: &Value) -> Kind {
    if let Some(c) = choices(schema) {
        return Kind::Choice(c);
    }
    let types: Vec<&str> = types(schema).into_iter().filter(|t| *t != "null").collect();
    match types.as_slice() {
        ["string"] => return Kind::Text,
        ["integer"] => return Kind::Integer,
        ["number"] => return Kind::Number,
        ["boolean"] => return Kind::Boolean,
        ["array"] if kind(&schema["items"]) == Kind::Text => return Kind::TextList,
        _ => {}
    }
    // A union whose every non-null branch is text is text ("self" or a UUID).
    let branches = schema["anyOf"].as_array().or(schema["oneOf"].as_array());
    if let Some(branches) = branches {
        let mut real = branches.iter().filter(|b| b["type"] != "null").peekable();
        if real.peek().is_some() && real.all(|b| matches!(kind(b), Kind::Text | Kind::Choice(_))) {
            return Kind::Text;
        }
    }
    Kind::Any
}
fn hint(schema: &Value) -> String {
    let range = |unit: &str| match (schema["minimum"].as_i64(), schema["maximum"].as_i64()) {
        (Some(a), Some(b)) => format!("{unit} {a}..{b}"),
        (Some(a), None) => format!("{unit} >= {a}"),
        (None, Some(b)) => format!("{unit} <= {b}"),
        (None, None) => unit.to_owned(),
    };
    match kind(schema) {
        Kind::Choice(c) => c.join("|"),
        Kind::Integer => range("integer"),
        Kind::Number => range("number"),
        Kind::Boolean => "true|false".into(),
        Kind::TextList => "text, repeatable".into(),
        Kind::Any => "json".into(),
        Kind::Text => match schema["format"].as_str() {
            Some(f) => f.to_owned(),
            None => "text".into(),
        },
    }
}

/// One generated flag.
#[derive(Debug, Clone)]
pub struct Flag {
    pub param: String,
    pub schema: &'static Value,
    pub required: bool,
}
impl Flag {
    pub fn name(&self) -> String {
        self.param.replace('_', "-")
    }
}
/// The generated flags of `name` for `principal`, required ones first.
pub fn flags(name: &str, principal: &str) -> Vec<Flag> {
    let Some(schema) = schema(name, principal) else {
        return vec![];
    };
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut flags: Vec<Flag> = schema["properties"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(param, _)| !RESERVED.contains(&param.as_str()))
        .map(|(param, schema)| Flag {
            param: param.clone(),
            schema,
            required: required.contains(&param.as_str()),
        })
        .collect();
    // The param that names an action's subject when the payload does not
    // (the server takes it out of the payload and builds the subject).
    let subject =
        &catalog::entry(name).map_or(&Value::Null, |e| &e["principals"][principal])["subject"];
    if let Some(param) = subject["id_param"].as_str()
        && subject["id_param_schema"].is_object()
        && !flags.iter().any(|f| f.param == param)
    {
        flags.push(Flag {
            param: param.to_owned(),
            schema: &subject["id_param_schema"],
            required: true,
        });
    }
    flags.sort_by_key(|f| !f.required);
    flags
}

fn invalid(message: &str) -> Value {
    error("invalid_arguments", message)
}
fn value(flag: &Flag, text: &str) -> Result<Value> {
    let bad = |what: &str| invalid(&format!("--{} takes {what}", flag.name()));
    Ok(match kind(flag.schema) {
        Kind::Text | Kind::TextList => json!(text),
        Kind::Choice(c) => {
            if !c.iter().any(|v| v == text) {
                return Err(bad(&format!("one of {}", c.join(", "))));
            }
            json!(text)
        }
        Kind::Integer => json!(text.parse::<i64>().map_err(|_| bad("an integer"))?),
        Kind::Number => {
            let n = text.parse::<f64>().ok().filter(|n| n.is_finite());
            json!(n.ok_or_else(|| bad("a number"))?)
        }
        Kind::Boolean => json!(text.parse::<bool>().map_err(|_| bad("true or false"))?),
        Kind::Any => serde_json::from_str::<crate::json::UniqueJson>(text)
            .map(|v| v.0)
            .unwrap_or_else(|_| json!(text)),
    })
}
/// Take the generated flags out of `args` (`--name VALUE` or `--name=VALUE`;
/// a boolean flag alone is `true`). Returns their params and the other
/// arguments, in order, for `run`'s own flag parser.
pub fn split(flags: &[Flag], args: &[String]) -> Result<(Map<String, Value>, Vec<String>)> {
    let mut params = Map::new();
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if arg == "--" {
            rest.extend_from_slice(&args[i - 1..]);
            break;
        }
        let (name, inline) = match arg.strip_prefix("--").map(|a| a.split_once('=')) {
            Some(Some((name, v))) => (name, Some(v)),
            Some(None) => (&arg[2..], None),
            None => ("", None),
        };
        let Some(flag) = flags.iter().find(|f| f.name() == name) else {
            rest.push(arg.clone());
            continue;
        };
        let kind = kind(flag.schema);
        let text = match inline {
            Some(v) => v.to_owned(),
            None if kind == Kind::Boolean => match args.get(i).map(String::as_str) {
                Some(v @ ("true" | "false")) => {
                    i += 1;
                    v.to_owned()
                }
                _ => "true".into(),
            },
            None => {
                let v = args.get(i).ok_or_else(|| {
                    invalid(&format!("--{name} takes a value ({})", hint(flag.schema)))
                })?;
                i += 1;
                v.clone()
            }
        };
        let v = value(flag, &text)?;
        if kind == Kind::TextList {
            let list = params.entry(flag.param.clone()).or_insert(json!([]));
            list.as_array_mut().expect("list").push(v);
        } else if params.insert(flag.param.clone(), v).is_some() {
            return Err(invalid(&format!("--{name} is given twice")));
        }
    }
    Ok((params, rest))
}
fn tokens(entry: &Value) -> String {
    let cli: Vec<&str> = entry["cli"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    cli.join(" ")
}
/// `aq <tokens> --help`: what the command does, who may run it and its flags.
pub fn help(name: &str, principal: &str) -> Option<String> {
    let entry = catalog::entry(name)?;
    let program = program();
    let mut out = String::new();
    let _ = writeln!(out, "{}", entry["summary"].as_str().unwrap_or_default());
    let _ = writeln!(out);
    let mut facts = vec![entry["kind"].as_str().unwrap_or("command").to_owned()];
    if let Some(grant) = entry["grant"].as_str() {
        facts.push(format!("needs {grant}"));
    }
    if entry["owner_only"] == true {
        facts.push("owner only".into());
    }
    let _ = writeln!(out, "Command: {name} ({})", facts.join(", "));
    let available: Vec<&str> = entry["principals"]
        .as_object()
        .into_iter()
        .flat_map(|p| p.keys())
        .map(String::as_str)
        .collect();
    if !available.contains(&principal) {
        let _ = writeln!(
            out,
            "Not available to the {principal}; it is a command of the {}.",
            available.join(" and the ")
        );
        return Some(out);
    }
    let flags = flags(name, principal);
    let action = entry["principals"][principal]["body"] == "action_request";
    // With a subject rule the subject follows from the params.
    let derived = entry["principals"][principal]["subject"].is_object();
    let mut usage = format!("{program} {}", tokens(entry));
    for f in flags.iter().filter(|f| f.required) {
        let _ = write!(usage, " --{} <{}>", f.name(), hint(f.schema));
    }
    if action && !derived {
        usage.push_str(" --subject TYPE:ID[:REVISION]");
    }
    let needs = |field: &str| {
        schema(name, principal)
            .and_then(|s| s["required"].as_array())
            .is_some_and(|r| r.iter().any(|v| v == field))
    };
    if !action && needs("reason") {
        usage.push_str(" --reason <text>");
    }
    if flags.iter().any(|f| !f.required) {
        usage.push_str(" [flags]");
    }
    let _ = writeln!(out, "Usage:   {usage}");
    if schema(name, principal).is_none() {
        let _ = writeln!(
            out,
            "\nIts params are not described in this binary; `{program} run {name} --describe` prints the published entry."
        );
    } else if !flags.is_empty() {
        let _ = writeln!(out, "\nParams:");
        let width = flags.iter().map(|f| f.name().len()).max().unwrap_or(0);
        for f in &flags {
            let mut notes = vec![hint(f.schema)];
            if f.required {
                notes.push("required".into());
            }
            if let Some(d) = f.schema.get("default").filter(|d| !d.is_null()) {
                notes.push(format!("default {}", d.to_string().trim_matches('"')));
            }
            let mut line = format!("  --{:<width$}  {}", f.name(), notes.join(", "));
            if let Some(d) = f.schema["description"].as_str() {
                let _ = write!(line, ". {d}");
            }
            let _ = writeln!(out, "{line}");
        }
    }
    // Params that share a name with one of `run`'s flags have no flag.
    let shadowed: Vec<String> = schema(name, principal)
        .and_then(|s| s["properties"].as_object())
        .into_iter()
        .flat_map(|p| p.keys())
        // A route-backed command's body takes `--reason` itself.
        .filter(|p| RESERVED.contains(&p.as_str()) && *p != "operation_id")
        .filter(|p| action || *p != "reason")
        .map(|p| format!("--param {p}=…"))
        .collect();
    if !shadowed.is_empty() {
        let _ = writeln!(
            out,
            "\nGiven by name (the flag of the same name is `run`'s own): {}",
            shadowed.join(", ")
        );
    }
    let _ = writeln!(out, "\nAlso:");
    let _ = writeln!(
        out,
        "  --param KEY=VALUE|KEY:=JSON   any param by name (repeatable); --params-file FILE|- for a JSON object"
    );
    if entry["kind"] == "mutation" {
        if action {
            let _ = writeln!(
                out,
                "  --subject TYPE:ID[:REVISION]  what the action changes{}; --expect TYPE=REVISION for other versions",
                if derived {
                    " (taken from the params when omitted)"
                } else {
                    ""
                }
            );
        }
        let _ = writeln!(
            out,
            "  --reason TEXT, --operation-id ID (a fresh ID is used when omitted)"
        );
    }
    let _ = writeln!(
        out,
        "  --json                        one JSON document; --describe prints the published entry"
    );
    Some(out)
}
/// The commands whose CLI tokens start with `words`, one line each. Empty
/// when `words` is not the start of any command.
pub fn group(words: &[String]) -> Vec<(String, String)> {
    let mut lines: Vec<(String, String)> = catalog::commands()
        .iter()
        .filter(|c| {
            c["cli"].as_array().is_some_and(|cli| {
                cli.len() > words.len() && cli.iter().zip(words).all(|(a, b)| a == b.as_str())
            })
        })
        .map(|c| {
            (
                tokens(c),
                c["summary"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    lines.sort();
    lines
}
/// `aq <group>`: the commands under those tokens.
pub fn group_help(words: &[String]) -> Option<String> {
    let lines = group(words);
    if lines.is_empty() {
        return None;
    }
    let program = program();
    let width = lines.iter().map(|(t, _)| t.len()).max().unwrap_or(0);
    let mut out = format!("Commands under `{program} {}`:\n\n", words.join(" "));
    for (tokens, summary) in &lines {
        let _ = writeln!(out, "  {tokens:<width$}  {summary}");
    }
    let _ = write!(
        out,
        "\n`{program} <command> --help` shows a command's flags; `{program} commands` lists what you may run."
    );
    Some(out)
}
/// The first tokens of every command, for the top-level help.
pub fn groups() -> Vec<String> {
    let mut groups: Vec<String> = catalog::commands()
        .iter()
        .filter_map(|c| c["cli"][0].as_str().map(str::to_owned))
        .collect();
    groups.sort();
    groups.dedup();
    groups
}

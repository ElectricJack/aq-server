use super::*;
pub(super) fn tokens(entry: &Value) -> String {
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
    let route = &entry["principals"][principal];
    let help = Help {
        name,
        entry,
        schema: schema(name, principal),
        flags: flags(name, principal),
        action: route["body"] == "action_request",
        derived: route["subject"].is_object(),
    };
    let _ = writeln!(out, "Usage:   {}", help.usage());
    help.params(&mut out);
    help.named_params(&mut out);
    help.generic_flags(&mut out);
    Some(out)
}
struct Help<'a> {
    name: &'a str,
    entry: &'static Value,
    schema: Option<&'static Value>,
    flags: Vec<Flag>,
    action: bool,
    derived: bool,
}
impl Help<'_> {
    fn usage(&self) -> String {
        let program = program();
        let mut usage = format!("{program} {}", tokens(self.entry));
        for f in self.flags.iter().filter(|f| f.required) {
            let _ = write!(usage, " --{} <{}>", f.name(), hint(f.schema));
        }
        if self.action && !self.derived {
            usage.push_str(" --subject TYPE:ID[:REVISION]");
        }
        let needs = |field: &str| {
            self.schema
                .and_then(|s| s["required"].as_array())
                .is_some_and(|r| r.iter().any(|v| v == field))
        };
        if !self.action && needs("reason") {
            usage.push_str(" --reason <text>");
        }
        if self.flags.iter().any(|f| !f.required) {
            usage.push_str(" [flags]");
        }
        usage
    }
    fn params(&self, out: &mut String) {
        let (program, name) = (program(), self.name);
        if self.schema.is_none() {
            let _ = writeln!(
                out,
                "\nIts params are not described in this binary; `{program} run {name} --describe` prints the published entry."
            );
        } else if !self.flags.is_empty() {
            let _ = writeln!(out, "\nParams:");
            let width = self.flags.iter().map(|f| f.name().len()).max().unwrap_or(0);
            for f in &self.flags {
                let line = f.description(width);
                let _ = writeln!(out, "{line}");
            }
        }
    }
    fn named_params(&self, out: &mut String) {
        // Params that share a name with one of `run`'s flags have no flag.
        let shadowed: Vec<String> = self
            .schema
            .and_then(|s| s["properties"].as_object())
            .into_iter()
            .flat_map(|p| p.keys())
            // A route-backed command's body takes `--reason` itself.
            .filter(|p| RESERVED.contains(&p.as_str()) && *p != "operation_id")
            .filter(|p| self.action || *p != "reason")
            .map(|p| format!("--param {p}=…"))
            .collect();
        if !shadowed.is_empty() {
            let _ = writeln!(
                out,
                "\nGiven by name (the flag of the same name is `run`'s own): {}",
                shadowed.join(", ")
            );
        }
    }
    fn generic_flags(&self, out: &mut String) {
        let _ = writeln!(out, "\nAlso:");
        let _ = writeln!(
            out,
            "  --param KEY=VALUE|KEY:=JSON   any param by name (repeatable); --params-file FILE|- for a JSON object"
        );
        if self.entry["kind"] == "mutation" {
            if self.action {
                let _ = writeln!(
                    out,
                    "  --subject TYPE:ID[:REVISION]  what the action changes{}; --expect TYPE=REVISION for other versions",
                    if self.derived {
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
    }
}
impl Flag {
    fn description(&self, width: usize) -> String {
        let mut notes = vec![hint(self.schema)];
        if self.required {
            notes.push("required".into());
        }
        if let Some(d) = self.schema.get("default").filter(|d| !d.is_null()) {
            notes.push(format!("default {}", d.to_string().trim_matches('"')));
        }
        let mut line = format!("  --{:<width$}  {}", self.name(), notes.join(", "));
        if let Some(d) = self.schema["description"].as_str() {
            let _ = write!(line, ". {d}");
        }
        line
    }
}

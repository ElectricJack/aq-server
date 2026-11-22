//! Human output. Server text is escaped so it cannot drive the terminal; JSON
//! output (`--json`) is the stable form for scripts and agents.
use serde_json::Value;
use std::fmt::Write;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Render {
    Json,
    Commands,
    Whoami,
    /// A conversation's messages page (`conversation messages`).
    Messages,
}
/// Escape control characters (and nothing else).
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            let _ = write!(out, "\\u{{{:x}}}", c as u32);
        } else {
            out.push(c);
        }
    }
    out
}
fn clean_lines(text: &str) -> String {
    let lines: Vec<String> = text.trim_end().lines().map(clean).collect();
    lines.join("\n")
}
fn s(v: &Value) -> String {
    match v {
        Value::String(s) => clean(s),
        Value::Null => "-".into(),
        other => clean(&other.to_string()),
    }
}
fn grant(g: &Value) -> String {
    let mut text = format!("{} @ {}", s(&g["operation"]), s(&g["scope"]));
    if let Some(limits) = g["limits"].as_object().filter(|l| !l.is_empty()) {
        let parts: Vec<_> = limits
            .iter()
            .map(|(k, v)| format!("{}={}", clean(k), s(v)))
            .collect();
        let _ = write!(text, " ({})", parts.join(", "));
    }
    text
}
pub fn render(kind: Render, value: &Value) -> String {
    match kind {
        // Generated help is text (surface::help).
        Render::Json => match value.as_object().filter(|o| o.len() == 1) {
            Some(o) if o.get("help").is_some_and(Value::is_string) => {
                clean_lines(value["help"].as_str().unwrap_or_default())
            }
            _ => serde_json::to_string_pretty(value).expect("JSON value"),
        },
        Render::Commands => commands(value),
        Render::Whoami => whoami(value),
        Render::Messages => messages(value),
    }
}
fn commands(value: &Value) -> String {
    let mut out = String::new();
    let row = |out: &mut String, c: &Value, last: String| {
        let cli: Vec<String> = c["cli"]
            .as_array()
            .map(|a| a.iter().map(s).collect())
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "{:<30} {:<8} {:<34} {} {}",
            s(&c["name"]),
            s(&c["kind"]),
            last,
            crate::program(),
            cli.join(" "),
        );
        let summary = s(&c["summary"]);
        if !summary.is_empty() && summary != "-" {
            let _ = writeln!(out, "    {summary}");
        }
    };
    let _ = writeln!(out, "principal: {}", s(&value["principal"]));
    let _ = writeln!(
        out,
        "{:<30} {:<8} {:<34} CLI",
        "COMMAND", "KIND", "GRANT @ SCOPE"
    );
    for c in value["commands"].as_array().into_iter().flatten() {
        let held = match (&c["grant"], &c["scope"]) {
            (Value::Null, _) => "(none needed)".to_owned(),
            (g, Value::Null) => s(g),
            (g, scope) => format!("{} @ {}", s(g), s(scope)),
        };
        row(&mut out, c, held);
    }
    if let Some(unavailable) = value["unavailable"].as_array().filter(|u| !u.is_empty()) {
        let _ = writeln!(out, "\nnot runnable:");
        for c in unavailable {
            row(
                &mut out,
                c,
                format!("{} ({})", s(&c["grant"]), s(&c["reason"])),
            );
        }
    }
    out.trim_end().to_owned()
}
/// Unix milliseconds as `YYYY-MM-DD HH:MM:SS UTC`.
fn utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (proleptic Gregorian).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}
/// A conversation's messages: one header line per message (cursor, author
/// kind, time), then its body indented. The conversation's revision is not
/// shown: it is not a message count.
fn messages(value: &Value) -> String {
    let mut out = String::new();
    let items = value["items"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "conversation {} ({}), {} message{} in this page",
        s(&value["conversation"]["id"]),
        s(&value["conversation"]["label"]),
        items.len(),
        if items.len() == 1 { "" } else { "s" },
    );
    for item in items {
        let time = item["created_at_ms"]
            .as_i64()
            .map_or_else(|| s(&item["created_at_ms"]), utc);
        let _ = writeln!(
            out,
            "{}  {}  {}",
            s(&item["cursor"]),
            s(&item["author"]["kind"]),
            time
        );
        for line in item["body"].as_str().unwrap_or_default().lines() {
            let _ = writeln!(out, "  {}", clean(line));
        }
    }
    if let Some(next) = value["next_cursor"]
        .as_str()
        .filter(|_| value["has_more"] == true)
    {
        let flag = if value["direction"] == "forward" {
            "--after"
        } else {
            "--before"
        };
        let _ = writeln!(out, "more: {flag} {}", clean(next));
    }
    out.trim_end().to_owned()
}
fn whoami(value: &Value) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "principal: {}", s(&value["principal"]));
    match value["principal"].as_str() {
        Some("agent") => {
            for (label, key) in [
                ("attempt", "attempt_id"),
                ("task", "task_id"),
                ("project", "project_id"),
                ("session", "session_id"),
                ("generation", "session_generation"),
                ("workspace", "workspace_id"),
                ("supervisor", "supervisor_conversation_id"),
                ("profile", "profile_id"),
            ] {
                let _ = writeln!(out, "{label:<10} {}", s(&value[key]));
            }
            let c = &value["credential"];
            let _ = writeln!(
                out,
                "{:<10} expires {} (version {}, authority revision {})",
                "credential",
                s(&c["expires_at"]),
                s(&c["version"]),
                s(&c["authority_revision"])
            );
            let _ = writeln!(out, "grants:");
            for g in value["grants"].as_array().into_iter().flatten() {
                let _ = writeln!(out, "  {}", grant(g));
            }
        }
        Some("supervisor") => {
            for key in ["session_id", "actor_id", "generation", "scopes"] {
                let _ = writeln!(out, "{key:<10} {}", s(&value[key]));
            }
        }
        _ => {
            for key in ["user_id", "email"] {
                let _ = writeln!(out, "{key:<10} {}", s(&value[key]));
            }
        }
    }
    out.trim_end().to_owned()
}
/// A refusal (§6.8) for stderr: the message, the grant and what is held.
pub fn refusal(e: &Value) -> String {
    let mut out = format!("refused ({}): {}", s(&e["code"]), s(&e["command"]));
    if !e["grant"].is_null() {
        let _ = write!(out, " needs {}", s(&e["grant"]));
    }
    let _ = write!(out, "\n{}", s(&e["message"]));
    let held: Vec<String> = e["held"]
        .as_array()
        .map(|h| h.iter().map(grant).collect())
        .unwrap_or_default();
    if !held.is_empty() {
        let _ = write!(out, "\nheld: {}", held.join(", "));
    }
    if e["request_id"].is_string() {
        let _ = write!(out, "\nrequest_id: {}", s(&e["request_id"]));
    }
    out
}

//! Interpret subprocess failures and database metadata.

use super::{RuntimeResult, failed};

/// The one stderr line a failure of a command run through `docker compose`
/// may show in history and `status.json`: the first line with `ERROR:` (else
/// the first with `error`, any case), without control characters and at
/// most 200 characters. Lines containing `CONTEXT:` or `=` are never used:
/// PostgreSQL puts row data there, and Compose quotes env files.
pub fn sanitized_stderr(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let usable: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains("CONTEXT:") && !l.contains('='))
        .collect();
    let line = usable
        .iter()
        .find(|l| l.contains("ERROR:"))
        .or_else(|| usable.iter().find(|l| l.to_lowercase().contains("error")))?;
    Some(line.chars().filter(|c| !c.is_control()).take(200).collect())
}

/// The error text for a failed command. Compose calls (everything run in
/// the containers, and anything that reads the env files) carry only the
/// sanitized line; their full stderr tail goes to the updater's own log.
pub(super) fn failure_text(
    name: &str,
    status: &std::process::ExitStatus,
    stderr: &[u8],
    compose: bool,
) -> String {
    if !compose {
        return format!("{name} failed ({status}): {}", stderr_tail(stderr));
    }
    tracing::warn!(command = name, %status, stderr = ?stderr_tail(stderr), "command failed");
    match sanitized_stderr(stderr) {
        Some(line) => format!("{name} failed ({status}): {line}"),
        None => format!("{name} failed ({status})"),
    }
}

/// The last 2 KiB of a child's stderr, for error messages.
pub fn stderr_tail(stderr: &[u8]) -> String {
    const MAX: usize = 2048;
    let mut tail = &stderr[stderr.len().saturating_sub(MAX)..];
    // Do not start in the middle of a UTF-8 sequence.
    while let [first, rest @ ..] = tail
        && (first & 0xC0) == 0x80
    {
        tail = rest;
    }
    let mut s = String::from_utf8_lossy(tail).trim().to_string();
    while s.len() > MAX {
        let mut cut = s.len() - MAX;
        while !s.is_char_boundary(cut) {
            cut += 1;
        }
        s.drain(..cut);
    }
    s
}

/// Interpret `psql -tAc <state format query>`: a missing database or `meta`
/// table means no state format yet.
pub fn parse_state_format(
    status: &std::process::ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
) -> RuntimeResult<Option<u32>> {
    if !status.success() {
        let err = String::from_utf8_lossy(stderr);
        let missing = err.contains("database \"aq_server\" does not exist")
            || err.contains("relation \"meta\" does not exist");
        return if missing {
            Ok(None)
        } else {
            Err(failed(failure_text(
                "psql (state format)",
                status,
                stderr,
                true,
            )))
        };
    }
    let text = String::from_utf8_lossy(stdout);
    let value = text.trim();
    if value.is_empty() {
        return Ok(None);
    }
    value.parse().map(Some).map_err(|_| {
        failed(format!(
            "unexpected state_format_version {value:.64?} in the database"
        ))
    })
}

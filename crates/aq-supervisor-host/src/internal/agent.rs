//! Agent mode (spec 0013 §4.2): the run's working directory is a workspace
//! holding the request's files, the harness works in it with tools, and its
//! answer is the outcome file it leaves there. This module writes the files,
//! reads the outcome back as untrusted data, and turns the harness's event
//! stream into a clipped transcript. It knows nothing about what the work is.
use super::{Failure, Place, failure};
use crate::subscription::Identity;
use aq_internal_work::{
    AGENT_OUTCOME, AGENT_TMP, AgentWork, Reason, TRANSCRIPT_BYTES, TRANSCRIPT_ITEM_BYTES,
    workspace_path,
};
use serde_json::{Map, Value, json};
use std::{
    ffi::CString,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::Path,
};

/// The failure detail of an agent that wrote no outcome.
pub(super) const NO_OUTCOME: &str = "the agent ended without saving, asking or giving up";
/// What replaces a login secret in text that leaves a run.
pub(super) const REDACTED: &str = "[credential redacted]";

/// The executables an agent's shell may run, and the only ones. Each is
/// installed under the host's `libexec` directory, off `PATH`, and reached
/// through its shim ([`shim`]). Its own contract (another task's): every path
/// argument is relative to the working directory, without `..` or a link.
pub(super) const SHELL_EXECUTABLES: [&str; 1] = ["aq-playbooks"];

/// The agent's temporary directory in its workspace.
pub(super) const TMP: &str = AGENT_TMP;

/// Prepares agent work's place under the fresh, host-owned run directories
/// `root` and `cwd`. The workspace is built while it is still the host's,
/// so nothing can be swapped into it, then handed to the agent's uid without
/// following links. `root` stays the host's (0711), with the agent's own
/// `home` and the root-owned `bin` of shims in it.
pub(super) fn prepare(
    root: &Path,
    cwd: &Path,
    work: &AgentWork,
    libexec: &Path,
    identity: Identity,
) -> std::io::Result<Place> {
    materialize(cwd, work)?;
    let tmp = cwd.join(TMP);
    std::fs::create_dir(&tmp)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700))?;
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o711))?;
    let home = root.join("home");
    std::fs::create_dir(&home)?;
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
    let bin = root.join("bin");
    std::fs::create_dir(&bin)?;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
    for name in SHELL_EXECUTABLES {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(bin.join(name))?
            .write_all(shim(name, cwd, &libexec.join(name))?.as_bytes())?;
        // `mode` is narrowed by the host's umask; the agent must run it.
        std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o755))?;
    }
    if identity.drop_privileges {
        hand_over(cwd, identity.run_as)?;
        hand_over(&home, identity.run_as)?;
    }
    Ok(Place {
        root: root.to_owned(),
        home,
        cwd: cwd.to_owned(),
    })
}

/// The shim of `name`: it runs `engine` only when its working directory is
/// the workspace `cwd`, with an environment of `PATH`, `HOME`, the
/// workspace's `TMPDIR` and a locale, so a login token in the harness's
/// environment never reaches it.
fn shim(name: &str, cwd: &Path, engine: &Path) -> std::io::Result<String> {
    let quoted = |path: &Path| match path.to_str() {
        Some(text) if !text.contains('\'') => Ok(format!("'{text}'")),
        _ => Err(std::io::Error::other("a path the shim cannot quote")),
    };
    let (workspace, tmp, engine) = (quoted(cwd)?, quoted(&cwd.join(TMP))?, quoted(engine)?);
    Ok(format!(
        "#!/bin/sh\n\
         # {name} for agent work: only in the workspace, with a clean environment.\n\
         if [ \"$(pwd -P)\" != {workspace} ]; then\n\
         \x20 echo '{name}: runs only in the workspace' >&2\n\
         \x20 exit 126\n\
         fi\n\
         exec /usr/bin/env -i PATH=/usr/local/bin:/usr/bin:/bin HOME=\"$HOME\" \
         TMPDIR={tmp} LANG=C.UTF-8 {engine} \"$@\"\n"
    ))
}

/// Gives `path` and, for a directory, everything in it to `uid`, never
/// following a link.
fn hand_over(path: &Path, uid: u32) -> std::io::Result<()> {
    std::os::unix::fs::lchown(path, Some(uid), Some(uid))?;
    if std::fs::symlink_metadata(path)?.is_dir() {
        for entry in std::fs::read_dir(path)? {
            hand_over(&entry?.path(), uid)?;
        }
    }
    Ok(())
}

/// Writes `work`'s files into the fresh workspace `cwd`: directories 0700 and
/// files 0600. A path that would leave the workspace is refused, as
/// `Request::validate` refuses it.
fn materialize(cwd: &Path, work: &AgentWork) -> std::io::Result<()> {
    for (path, text) in &work.files {
        if !workspace_path(path) {
            return Err(std::io::Error::other("workspace path refused"));
        }
        let (dirs, name) = path.rsplit_once('/').unwrap_or(("", path));
        let mut dir = cwd.to_path_buf();
        for segment in dirs.split('/').filter(|s| !s.is_empty()) {
            dir.push(segment);
            match std::fs::create_dir(&dir) {
                Ok(()) => {
                    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
                }
                // Made for an earlier file; never a link, the workspace is new.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if !std::fs::symlink_metadata(&dir)?.is_dir() {
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            }
        }
        let file = dir.join(name);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&file)?
            .write_all(text.as_bytes())?;
    }
    Ok(())
}

/// The text of [`AGENT_OUTCOME`] in the workspace `cwd`, or `None` when the
/// agent wrote none. Every component is opened without following links, so
/// a link anywhere on the path refuses the outcome instead of reading what it
/// points to. More than `limit` bytes, not a regular file, or not UTF-8 is
/// refused too, with the reason.
pub(super) fn read_outcome(cwd: &Path, limit: u64) -> Result<Option<String>, String> {
    let directory = libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let mut at = open_at(None, cwd.as_os_str().as_bytes(), directory)
        .map_err(|_| "the workspace could not be read".to_owned())?;
    let mut segments = AGENT_OUTCOME.split('/').peekable();
    while let Some(segment) = segments.next() {
        let last = segments.peek().is_none();
        let flags = match last {
            true => libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            false => directory,
        };
        at = match open_at(Some(&at), segment.as_bytes(), flags) {
            Ok(fd) => fd,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
            Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
                return Err(format!(
                    "the outcome path {AGENT_OUTCOME} is a link or not a file"
                ));
            }
            Err(_) => return Err("the outcome file could not be opened".into()),
        };
    }
    let mut file = std::fs::File::from(at);
    let meta = file
        .metadata()
        .map_err(|_| "the outcome file could not be read".to_owned())?;
    if !meta.is_file() {
        return Err(format!(
            "the outcome path {AGENT_OUTCOME} is not a regular file"
        ));
    }
    if meta.nlink() != 1 {
        return Err("the outcome file has another link".into());
    }
    let over = |len: u64| format!("the outcome file is {len} bytes, over the limit of {limit}");
    if meta.size() > limit {
        return Err(over(meta.size()));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "the outcome file could not be read".to_owned())?;
    if bytes.len() as u64 > limit {
        return Err(over(bytes.len() as u64));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| "the outcome file is not UTF-8".into())
}

fn open_at(dir: Option<&OwnedFd>, name: &[u8], flags: i32) -> std::io::Result<OwnedFd> {
    let name = CString::new(name).map_err(|_| std::io::Error::other("NUL in a path"))?;
    let at = dir.map_or(libc::AT_FDCWD, |fd| fd.as_raw_fd());
    let fd = unsafe { libc::openat(at, name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The agent's raw answer: the outcome file's text through `redact`. Without
/// an outcome it is the harness's own failure when it reported one (a usage
/// limit moves on to the next candidate), else [`NO_OUTCOME`]. An outcome
/// that cannot be read safely is invalid output.
pub(super) fn conclude(
    cwd: &Path,
    limit: u64,
    harness_failed: Option<Failure>,
    redact: impl Fn(String) -> Option<String>,
) -> Result<String, Failure> {
    match read_outcome(cwd, limit) {
        Ok(Some(text)) => redact(text)
            .ok_or_else(|| failure(Reason::ProviderError, "agent outcome could not be redacted")),
        Ok(None) => {
            Err(harness_failed.unwrap_or_else(|| failure(Reason::InvalidOutput, NO_OUTCOME)))
        }
        Err(detail) => Err((Reason::InvalidOutput, detail)),
    }
}

/// Splits a native's stdout into lines as it arrives, so an agent's event
/// stream is read whole without being held whole. A line longer than
/// `MAX_LINE` bytes is skipped.
#[derive(Default)]
pub(super) struct Lines {
    line: Vec<u8>,
    skipping: bool,
}

const MAX_LINE: usize = 8 * 1024 * 1024;

impl Lines {
    pub(super) fn feed(&mut self, mut chunk: &[u8], each: &mut dyn FnMut(&[u8])) {
        while let Some(end) = chunk.iter().position(|b| *b == b'\n') {
            if !self.skipping {
                self.line.extend_from_slice(&chunk[..end]);
                each(&self.line);
            }
            self.line.clear();
            self.skipping = false;
            chunk = &chunk[end + 1..];
        }
        if !self.skipping {
            self.line.extend_from_slice(chunk);
            if self.line.len() > MAX_LINE {
                self.line = Vec::new();
                self.skipping = true;
            }
        }
    }

    pub(super) fn end(mut self, each: &mut dyn FnMut(&[u8])) {
        if !self.skipping && !self.line.is_empty() {
            each(&std::mem::take(&mut self.line));
        }
    }
}

/// How much of one text an item keeps before it is redacted and clipped to
/// [`TRANSCRIPT_ITEM_BYTES`]: enough that any login secret starting within
/// the clipped item is held whole when it is redacted (a login file is at
/// most 64 KiB).
const SECRET_SLACK: usize = 64 * 1024;

/// What an agent did, as harness-neutral items: `text` (what it said),
/// `tool` (a tool it called and with what), `result` (what the tool
/// returned, and whether it failed) and `error`. Each is at most
/// [`TRANSCRIPT_ITEM_BYTES`] and the whole at most [`TRANSCRIPT_BYTES`];
/// a transcript cut short ends with `{"kind": "clipped"}`.
#[derive(Default)]
pub(super) struct Transcript {
    items: Vec<Map<String, Value>>,
    /// The size the items will have at most once clipped.
    size: usize,
    clipped: bool,
}

impl Transcript {
    /// One event of Claude Code's `--output-format stream-json`.
    pub(super) fn claude(&mut self, event: &Value) {
        let blocks = event["message"]["content"].as_array().into_iter().flatten();
        for block in blocks {
            match (event["type"].as_str(), block["type"].as_str()) {
                (Some("assistant"), Some("text")) => self.text("text", &block["text"]),
                (Some("assistant"), Some("tool_use")) => {
                    self.tool(&text_of(&block["name"]), &block["input"])
                }
                (Some("user"), Some("tool_result")) => {
                    let failed = block["is_error"].as_bool().unwrap_or(false);
                    self.result(&block["content"], failed)
                }
                _ => {}
            }
        }
    }

    /// One event of `codex exec --json`.
    pub(super) fn codex(&mut self, event: &Value) {
        if event["type"] != "item.completed" {
            return;
        }
        let item = &event["item"];
        match item["type"].as_str() {
            Some("agent_message") => self.text("text", &item["text"]),
            Some("error") => self.text("error", &item["message"]),
            Some("command_execution") => {
                self.tool("shell", &item["command"]);
                let failed = item["exit_code"].as_i64().is_some_and(|code| code != 0)
                    || item["status"] == "failed";
                self.result(&item["aggregated_output"], failed);
            }
            Some("file_change") => self.tool("file_change", &item["changes"]),
            Some("web_search") => self.tool("web_search", &item["query"]),
            Some("mcp_tool_call") => {
                let name = format!("{}.{}", text_of(&item["server"]), text_of(&item["tool"]));
                self.tool(&name, &item["arguments"]);
                let failed = !item["error"].is_null();
                let output = if failed {
                    &item["error"]
                } else {
                    &item["result"]
                };
                self.result(output, failed);
            }
            _ => {}
        }
    }

    fn text(&mut self, kind: &str, text: &Value) {
        self.push(json!({"kind": kind, "text": kept(&text_of(text))}));
    }

    fn tool(&mut self, name: &str, input: &Value) {
        self.push(json!({"kind": "tool", "tool": kept(name), "input": kept(&text_of(input))}));
    }

    fn result(&mut self, output: &Value, failed: bool) {
        self.push(json!({"kind": "result", "output": kept(&text_of(output)), "error": failed}));
    }

    fn push(&mut self, item: Value) {
        let Value::Object(item) = item else { return };
        if self.clipped {
            return;
        }
        let size = Value::Object(item.clone())
            .to_string()
            .len()
            .min(TRANSCRIPT_ITEM_BYTES);
        if self.size + size + 1 > TRANSCRIPT_BYTES {
            self.clipped = true;
            return;
        }
        self.size += size + 1;
        self.items.push(item);
    }

    /// The stored transcript: every text through `redact`, then each item
    /// clipped to [`TRANSCRIPT_ITEM_BYTES`] and the whole to
    /// [`TRANSCRIPT_BYTES`]. `None` when a text could not be redacted, so
    /// nothing unredacted leaves the run.
    pub(super) fn finish(self, redact: impl Fn(&str) -> Option<String>) -> Option<Value> {
        const CLIPPED: &str = r#"{"kind":"clipped"}"#;
        let mut kept = Vec::new();
        let mut size = 2;
        let mut clipped = self.clipped;
        for mut item in self.items {
            for value in item.values_mut() {
                if let Value::String(text) = value {
                    *text = redact(text)?;
                }
            }
            let item = fit(item);
            let item_size = item.to_string().len() + 1;
            if size + item_size + CLIPPED.len() + 1 > TRANSCRIPT_BYTES {
                clipped = true;
                break;
            }
            size += item_size;
            kept.push(item);
        }
        if clipped {
            kept.push(json!({"kind": "clipped"}));
        }
        Some(Value::Array(kept))
    }
}

/// `text` cut to what an item keeps before redaction.
fn kept(text: &str) -> String {
    super::clip(text.to_owned(), TRANSCRIPT_ITEM_BYTES + SECRET_SLACK)
}

/// A string as itself, an array of content blocks as their texts, anything
/// else as compact JSON.
fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Array(blocks) if blocks.iter().all(|b| b["text"].is_string()) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

/// `item` with its longest texts shortened until it is at most
/// [`TRANSCRIPT_ITEM_BYTES`] as JSON, marked `"clipped": true` if cut.
fn fit(mut item: Map<String, Value>) -> Value {
    loop {
        let size = Value::Object(item.clone()).to_string().len();
        if size <= TRANSCRIPT_ITEM_BYTES {
            return Value::Object(item);
        }
        item.insert("clipped".into(), Value::Bool(true));
        let longest = item
            .values_mut()
            .filter_map(|v| match v {
                Value::String(text) => Some(text),
                _ => None,
            })
            .max_by_key(|text| text.len());
        let Some(text) = longest.filter(|text| !text.is_empty()) else {
            return Value::Object(item);
        };
        // Escapes make JSON longer than the text, so cut by the excess.
        let excess = size - TRANSCRIPT_ITEM_BYTES + 16;
        let keep = text.len().saturating_sub(excess);
        *text = super::clip(std::mem::take(text), keep);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_split_across_chunks_and_an_overlong_line_is_skipped() {
        let mut lines = Lines::default();
        let mut seen = Vec::new();
        let mut each = |line: &[u8]| seen.push(String::from_utf8(line.to_vec()).unwrap());
        lines.feed(b"one\ntw", &mut each);
        lines.feed(b"o\n", &mut each);
        lines.feed(&vec![b'x'; MAX_LINE + 1], &mut each);
        lines.feed(b"x\nthree", &mut each);
        lines.end(&mut each);
        assert_eq!(seen, ["one", "two", "three"]);
    }

    #[test]
    fn an_item_is_clipped_to_its_bound_even_when_escapes_lengthen_it() {
        let item = json!({"kind": "result", "output": "\"\n".repeat(5000), "error": false});
        let Value::Object(item) = item else {
            unreachable!()
        };
        let fitted = fit(item);
        assert!(fitted.to_string().len() <= TRANSCRIPT_ITEM_BYTES);
        assert_eq!(fitted["clipped"], true);
        assert_eq!(fitted["kind"], "result");
    }

    #[test]
    fn a_secret_cut_by_the_item_clip_is_still_redacted_whole() {
        let mut transcript = Transcript::default();
        let secret = "s".repeat(4000);
        let text = format!("{}{secret}", "a".repeat(2000));
        transcript.claude(&json!({"type": "assistant",
            "message": {"content": [{"type": "text", "text": text}]}}));
        let events = transcript
            .finish(|text| Some(text.replace(&secret, REDACTED)))
            .unwrap();
        let text = events[0]["text"].as_str().unwrap();
        assert!(!text.contains("ssssssss"), "{text}");
        assert!(text.ends_with(REDACTED), "{text}");
    }
}

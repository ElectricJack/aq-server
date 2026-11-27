//! What failed, read from a failed job's log line by line as it streams in,
//! so that the next attempt sees the failing tests and not only the end of
//! the job: cargo-nextest's `FAIL [...]` and `Summary [...]` lines, each
//! `thread '...' panicked at <file>:<line>` line with the lines of its message
//! (assertion, left, right), rustc's `error[...]` and `error:` lines with
//! their `-->` location, and GitHub's `##[error]` lines. A log with none of
//! them keeps its tail. Lines lose GitHub's timestamp prefix and their
//! control sequences; the caller still redacts the result
//! ([`super::log_tail::log_tail`]).
use super::log_tail::strip_controls;
use std::collections::{BTreeSet, VecDeque};

/// Lines of a panic's message kept after its `panicked at` line.
const PANIC_LINES: usize = 5;
/// Lines after a compiler error searched for its `-->` location.
const LOCATION_LINES: usize = 3;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Line,
    Panic,
    Error,
}

/// The excerpt of one log, at most `max` bytes: whole entries, first come
/// first kept, each once.
pub(super) struct Excerpt {
    max: usize,
    kept: String,
    seen: BTreeSet<String>,
    entry: String,
    kind: Kind,
    /// Lines the open entry may still take.
    after: usize,
    tail: VecDeque<String>,
    tail_bytes: usize,
}

impl Excerpt {
    pub(super) fn new(max: usize) -> Self {
        Self {
            max,
            kept: String::new(),
            seen: BTreeSet::new(),
            entry: String::new(),
            kind: Kind::Line,
            after: 0,
            tail: VecDeque::new(),
            tail_bytes: 0,
        }
    }

    /// One line of the log, with or without its line end.
    pub(super) fn line(&mut self, raw: &str) {
        let mut line = clean(raw);
        cut(&mut line, self.max);
        let text = line.trim();
        if let Some(kind) = kind(text) {
            self.close();
            self.entry = text.to_owned();
            self.kind = kind;
            self.after = match kind {
                Kind::Line => 0,
                Kind::Panic => PANIC_LINES,
                Kind::Error => LOCATION_LINES,
            };
        } else if self.after > 0 {
            let location = text.starts_with("-->");
            let ends = text.is_empty()
                || text.starts_with("note: ")
                || text.starts_with("stack backtrace:");
            if ends {
                self.after = 0;
            } else if self.kind == Kind::Panic || location {
                self.entry.push('\n');
                self.entry.push_str(line.trim_end());
                self.after = if location { 0 } else { self.after - 1 };
            } else {
                self.after -= 1;
            }
        }
        if self.after == 0 {
            self.close();
        }
        self.remember(line);
    }

    /// The excerpt, or the log's tail when nothing in it matched.
    pub(super) fn finish(mut self) -> String {
        self.close();
        if self.kept.is_empty() {
            Vec::from(self.tail).join("\n")
        } else {
            self.kept
        }
    }

    fn close(&mut self) {
        let entry = std::mem::take(&mut self.entry);
        let fits = self.kept.len() + entry.len() < self.max;
        if !entry.is_empty() && fits && !self.seen.contains(&entry) {
            if !self.kept.is_empty() {
                self.kept.push('\n');
            }
            self.kept.push_str(&entry);
            self.seen.insert(entry);
        }
    }

    fn remember(&mut self, line: String) {
        if !self.kept.is_empty() {
            self.tail.clear();
            return;
        }
        self.tail_bytes += line.len() + 1;
        self.tail.push_back(line);
        while self.tail_bytes > self.max + 1 {
            let Some(old) = self.tail.pop_front() else {
                break;
            };
            self.tail_bytes -= old.len() + 1;
        }
    }
}

fn kind(text: &str) -> Option<Kind> {
    const NEXTEST: [&str; 6] = [
        "FAIL [",
        "TIMEOUT [",
        "SIGSEGV [",
        "SIGABRT [",
        "ABORT [",
        "Summary [",
    ];
    if text.starts_with("thread '") && text.contains("' panicked at ") {
        Some(Kind::Panic)
    } else if text.starts_with("error[E") || text.starts_with("error:") {
        Some(Kind::Error)
    } else if NEXTEST.iter().any(|p| text.starts_with(p))
        || (text.starts_with("TRY ") && text.contains(" FAIL ["))
        || text.starts_with("##[error]")
    {
        Some(Kind::Line)
    } else {
        None
    }
}

/// `raw` without its line end, control sequences and GitHub's
/// `2026-11-27T09:32:01.9700000Z ` timestamp prefix.
fn clean(raw: &str) -> String {
    let line = strip_controls(raw);
    let line = line.trim_end_matches('\n');
    let stamped = line.split_once(' ').filter(|(stamp, _)| {
        let b = stamp.as_bytes();
        b.len() >= 20
            && b[..4].iter().all(u8::is_ascii_digit)
            && b[4] == b'-'
            && b[10] == b'T'
            && stamp.ends_with('Z')
    });
    stamped.map_or(line, |(_, rest)| rest).to_owned()
}

/// `line` cut to at most `max` bytes, on a character boundary.
fn cut(line: &mut String, max: usize) {
    if line.len() > max {
        let mut end = max;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn excerpt(log: &str, max: usize) -> String {
        let mut e = Excerpt::new(max);
        log.split_inclusive('\n').for_each(|l| e.line(l));
        e.finish()
    }

    /// Each line as GitHub serves a job log: a timestamp, then the step's
    /// output with its colours and CRLF line ends.
    fn github(lines: &[&str]) -> String {
        lines
            .iter()
            .map(|l| format!("2026-11-27T09:32:01.9700000Z {l}\r\n"))
            .collect()
    }

    #[test]
    fn a_nextest_log_keeps_the_failed_tests_and_their_panics_once() {
        let log = github(&[
            "##[group]Run cargo nextest run --profile ci",
            "\u{1b}[32m        PASS\u{1b}[0m [   0.120s] aq-server::it_playbook playbook_host9::delivery::ok",
            "\u{1b}[31m        FAIL\u{1b}[0m [   1.034s] aq-server::it_playbook playbook_host9::delivery::checks_failed_posts",
            "\u{1b}[31m──── \u{1b}[0mSTDERR:             aq-server::it_playbook playbook_host9::delivery::checks_failed_posts",
            "",
            "thread 'playbook_host9::delivery::checks_failed_posts' panicked at crates/aq-server/tests/it_playbook/playbook_host9/delivery.rs:212:5:",
            "assertion `left == right` failed: the comment names the test",
            "  left: None",
            " right: Some(\"posted\")",
            "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace",
            "",
            "        PASS [   0.020s] aq-server::it_playbook playbook_host9::delivery::other",
            // The final output repeats the failure.
            "        FAIL [   1.034s] aq-server::it_playbook playbook_host9::delivery::checks_failed_posts",
            "thread 'playbook_host9::delivery::checks_failed_posts' panicked at crates/aq-server/tests/it_playbook/playbook_host9/delivery.rs:212:5:",
            "assertion `left == right` failed: the comment names the test",
            "  left: None",
            " right: Some(\"posted\")",
            "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace",
            "------------",
            "     Summary [  42.000s] 2 tests run: 1 passed, 1 failed, 0 skipped",
            "        FAIL [   1.034s] aq-server::it_playbook playbook_host9::delivery::checks_failed_posts",
            "error: test run failed",
            "##[error]Process completed with exit code 100.",
            "Post job cleanup.",
        ]);
        assert_eq!(
            excerpt(&log, 4096),
            "FAIL [   1.034s] aq-server::it_playbook playbook_host9::delivery::checks_failed_posts\n\
             thread 'playbook_host9::delivery::checks_failed_posts' panicked at crates/aq-server/tests/it_playbook/playbook_host9/delivery.rs:212:5:\n\
             assertion `left == right` failed: the comment names the test\n  \
             left: None\n \
             right: Some(\"posted\")\n\
             Summary [  42.000s] 2 tests run: 1 passed, 1 failed, 0 skipped\n\
             error: test run failed\n\
             ##[error]Process completed with exit code 100."
        );
    }

    #[test]
    fn a_compile_error_keeps_its_location() {
        let log = github(&[
            "   Compiling aq-delivery v0.1.0 (/home/runner/work/aq-server/crates/aq-delivery)",
            "\u{1b}[0m\u{1b}[1m\u{1b}[38;5;9merror[E0425]\u{1b}[0m\u{1b}[0m\u{1b}[1m: cannot find value `room` in this scope\u{1b}[0m",
            "\u{1b}[0m   \u{1b}[0m\u{1b}[0m\u{1b}[1m\u{1b}[38;5;12m--> \u{1b}[0m\u{1b}[0mcrates/aq-delivery/src/github/pulls.rs:301:21\u{1b}[0m",
            "    |",
            "301 |     let x = room;",
            "    |             ^^^^ not found in this scope",
            "",
            "error: could not compile `aq-delivery` (lib) due to 1 previous error",
        ]);
        assert_eq!(
            excerpt(&log, 4096),
            "error[E0425]: cannot find value `room` in this scope\n   \
             --> crates/aq-delivery/src/github/pulls.rs:301:21\n\
             error: could not compile `aq-delivery` (lib) due to 1 previous error"
        );
    }

    #[test]
    fn a_log_with_nothing_recognized_keeps_its_tail_within_the_bound() {
        let lines: Vec<String> = (0..100).map(|i| format!("step output {i:03}")).collect();
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let tail = excerpt(&github(&lines), 60);
        assert!(tail.len() <= 60, "{}", tail.len());
        assert!(tail.ends_with("step output 099"), "{tail}");
        assert!(tail.starts_with("step output "), "whole lines: {tail}");
    }

    #[test]
    fn the_excerpt_keeps_whole_entries_within_the_bound() {
        let lines: Vec<String> = (0..50)
            .map(|i| format!("        FAIL [   0.001s] crate::bin test_{i:02}"))
            .collect();
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let kept = excerpt(&github(&lines), 100);
        assert!(kept.len() <= 100, "{}", kept.len());
        assert_eq!(
            kept,
            "FAIL [   0.001s] crate::bin test_00\nFAIL [   0.001s] crate::bin test_01"
        );
        // A line longer than the bound is cut, never kept whole.
        let long = format!("FAIL [ {} ]", "é".repeat(100));
        assert!(excerpt(&long, 51).len() <= 51);
    }
}

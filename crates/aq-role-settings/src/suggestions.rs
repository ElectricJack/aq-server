//! Model suggestions for the level editor: the models a host tool discovered
//! per harness (`scripts/refresh-model-catalog.py` writes the cache file).
//! They are suggestions only: any model a level names is allowed, and nothing
//! here is read when an offer is made.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Read, path::Path};

const MAX_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;

/// One line of the cache file, a JSON array of these.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    provider: String,
    model: String,
    harness: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    observed_at: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Model {
    pub provider: String,
    pub model: String,
    pub source: Option<String>,
    pub observed_at: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Harness {
    pub harness: String,
    pub models: Vec<Model>,
}

/// The suggestions in `path`, by harness name and then in file order, each
/// (provider, model) once per harness. A missing, oversized or malformed
/// file gives none: a suggestion that cannot be read is not an error.
pub fn read(path: Option<&Path>) -> Vec<Harness> {
    let entries = || -> Option<Vec<Entry>> {
        let mut bytes = Vec::new();
        let file = std::fs::File::open(path?).ok()?;
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 > MAX_BYTES {
            return None;
        }
        let entries: Vec<Entry> = serde_json::from_slice(&bytes).ok()?;
        (entries.len() <= MAX_ENTRIES).then_some(entries)
    };
    let mut harnesses = BTreeMap::<String, Vec<Model>>::new();
    for entry in entries().unwrap_or_default() {
        let models = harnesses.entry(entry.harness).or_default();
        if !models
            .iter()
            .any(|m| m.provider == entry.provider && m.model == entry.model)
        {
            models.push(Model {
                provider: entry.provider,
                model: entry.model,
                source: entry.source,
                observed_at: entry.observed_at,
            });
        }
    }
    harnesses
        .into_iter()
        .map(|(harness, models)| Harness { harness, models })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, text: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "aq-model-suggestions-{}-{name}.json",
            std::process::id()
        ));
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn suggestions_are_grouped_by_harness_and_listed_once() {
        let path = file(
            "present",
            r#"[{"provider":"openai","model":"gpt-x","harness":"codex","source":"Codex CLI","observed_at":"2026-11-16T00:00:00Z"},
               {"provider":"anthropic","model":"claude-y","harness":"claude"},
               {"provider":"openai","model":"gpt-x","harness":"codex"}]"#,
        );
        let read = read(Some(&path));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            serde_json::to_value(&read).unwrap(),
            serde_json::json!([
                {"harness":"claude","models":[{"provider":"anthropic","model":"claude-y","source":null,"observed_at":null}]},
                {"harness":"codex","models":[{"provider":"openai","model":"gpt-x","source":"Codex CLI","observed_at":"2026-11-16T00:00:00Z"}]},
            ])
        );
    }

    #[test]
    fn an_absent_or_malformed_file_suggests_nothing() {
        assert!(read(None).is_empty());
        assert!(read(Some(Path::new("/nonexistent/aq-model-suggestions.json"))).is_empty());
        for (name, text) in [
            ("not-json", "{"),
            (
                "old-format",
                r#"[{"provider":"p","model":"m","harness":"h","preference_only":true}]"#,
            ),
        ] {
            let path = file(name, text);
            let read = read(Some(&path));
            std::fs::remove_file(&path).unwrap();
            assert!(read.is_empty(), "{name}");
        }
    }
}

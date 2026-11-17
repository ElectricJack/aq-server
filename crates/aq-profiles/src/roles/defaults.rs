//! The shipped default documents (`crates/aq-profiles/defaults`). The server
//! installs one when no document of that id exists (spec 0014 §7).
use super::{Kind, SourceDocument};

macro_rules! documents {
    ($($kind:ident $dir:literal: [$($id:literal),* $(,)?]),* $(,)?) => {
        vec![$($(SourceDocument {
            kind: Kind::$kind,
            label: concat!($dir, "/", $id, ".md").into(),
            text: include_str!(concat!("../../defaults/", $dir, "/", $id, ".md")).into(),
        }),*),*]
    };
}
/// Every shipped document, labelled with its path under `defaults/`.
pub fn default_documents() -> Vec<SourceDocument> {
    documents![
        Role "roles": ["developer", "reviewer"],
        Intelligence "intelligence": [
            "simple-low",
            "simple-high",
            "standard-low",
            "standard-medium",
            "standard-high",
            "complex-low",
            "complex-high",
            "deep-low",
            "deep-high",
        ],
        Execution "execution": ["sandbox"],
    ]
}

/// The platform text of an environment (spec 0014 §6.1): what is true of
/// every task that runs in it, and nothing about any project. Owned by the
/// server and shipped as Markdown under `defaults/platform`, one file named
/// for the environment id; an environment without its own file gets
/// `default.md`. The text is delivered trimmed.
pub fn platform_text(environment: &str) -> &'static str {
    match environment {
        "aq-ms1-docker-local-claude" => {
            include_str!("../../defaults/platform/aq-ms1-docker-local-claude.md")
        }
        _ => include_str!("../../defaults/platform/default.md"),
    }
    .trim()
}

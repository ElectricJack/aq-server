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
            "fast-low",
            "fast-high",
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

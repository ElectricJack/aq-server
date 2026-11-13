//! The reference compiled policy: what docs/specs/0011-routing-policy-default.md
//! compiles to against the owner's installation. For tests here and in other
//! crates (feature `fixtures`).

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::Class;
use crate::classify::{Offer, Options};
use crate::policy::Policy;

/// The classes the owner's installation has a profile assigned to.
pub const ASSIGNED_CLASSES: [&str; 9] = [
    "simple-low",
    "simple-high",
    "standard-low",
    "standard-medium",
    "standard-high",
    "complex-low",
    "complex-high",
    "deep-low",
    "deep-high",
];

pub const CLASSIFIER_INSTRUCTIONS: &str = "\
Classify one task for routing. Answer with its kind of work, the level it needs, its risk and three flags. \
Do not choose who takes the task.

Kind of work (task_type): feature, bugfix (fixing a defect), refactor, test, docs, chore, sync, \
spec (writing a specification), design, plan (an implementation plan), review (reviewing someone else's work), \
research, analysis, art (art and visual design). When you can't tell, answer feature.

Level (intelligence_class): features, bug fixes, refactors, tests, docs and research are standard-high; \
most tasks belong here. Chores and syncs are simple-high. Specs, designs, implementation plans and reviews \
of someone else's work are complex-high. Research is standard-high unless it is clearly harder. \
Answer a deep level only for analysis, planning or spec writing that needs advanced mathematics or \
unusually hard reasoning. A big task is not a deep task, and neither is a risky one.

Risk: how much damage the change could do if it is wrong. Judge it separately from how hard the work is \
and from how well the task is written. high: merging and publishing, CI gating, crash and session recovery, \
git branch handling, database migrations and stored data, authentication, permissions and secrets, \
money and live trading, deleting anything. very_high: such work when a mistake would be hard to undo or \
would reach outside the repository. medium: ordinary changes to shared code that other work depends on. \
low: docs, UI copy, isolated components and tests, where a mistake is visible and cheap to undo. \
When a task touches several areas, go by the riskiest. When you can't tell, answer medium and say in \
risk_reason that the risk was assumed.

Flags: narrow is true when the requirements are settled and the files to change are named or obvious. \
test_verified is true when an existing or specified test checks the result; work that needs manual or \
end-to-end verification is not covered. deep_reasoning is true only when the work needs advanced \
mathematics or unusually hard reasoning.

Never let how busy providers are or the task's priority affect any answer. \
Keep risk_reason and reason under 400 characters each.";

fn identity(classes: &[&str]) -> Value {
    classes.iter().map(|c| (c.to_string(), json!(c))).collect()
}

/// The reference policy and the options it was compiled against.
pub fn reference() -> (Policy, Options) {
    let all = identity(&ASSIGNED_CLASSES);
    let kind = |class: &str, max: &str, narrow: bool| json!({"class": class, "max_class": max, "narrow": narrow});
    let mut kinds = json!({
        "feature": kind("standard-high", "complex-high", true),
        "bugfix": kind("standard-high", "complex-high", true),
        "refactor": kind("standard-high", "complex-high", true),
        "test": kind("standard-high", "standard-high", true),
        "docs": kind("standard-high", "standard-high", true),
        "chore": kind("simple-high", "standard-high", true),
        "sync": kind("simple-high", "standard-high", true),
        "research": kind("standard-high", "complex-high", true),
        "spec": kind("complex-high", "deep-high", true),
        "design": kind("complex-high", "deep-high", false),
        "plan": kind("complex-high", "deep-high", false),
        "review": kind("complex-high", "deep-high", false),
        "analysis": kind("complex-high", "deep-high", false),
        "art": kind("standard-high", "complex-high", false),
    });
    kinds["bugfix"]["escalate"] = json!({"after_failures": 1, "to_class": "complex-high"});
    for (id, lane) in [
        ("design", "design"),
        ("plan", "planning"),
        ("review", "deep-analysis"),
        ("analysis", "deep-analysis"),
        ("art", "art"),
    ] {
        kinds[id]["lane"] = json!(lane);
    }
    for id in ["design", "plan"] {
        kinds[id]["prefer_harnesses"] = json!(["claude"]);
    }
    let claude_or_codex = json!(["claude", "codex"]);
    let policy = json!({
        "version": 2,
        "default_kind": "feature",
        "kinds": kinds,
        "origins": {"repair": {"narrow": false, "lane": "repair"}},
        "risk": {
            "medium": {"min_class": "standard-high", "harnesses": claude_or_codex},
            "high": {"min_class": "standard-high", "harnesses": claude_or_codex},
            "very_high": {
                "min_class": "complex-low",
                "harnesses": claude_or_codex,
                "relax": {"class": "standard-high", "requires": ["narrow", "test_verified"]}
            }
        },
        "lanes": [
            {
                "id": "local-first", "cheap": true,
                "classes": identity(&["simple-low", "simple-high", "standard-low", "standard-medium", "standard-high"]),
                "requires": ["narrow", "test_verified"], "max_risk": "low",
                "harnesses": ["opencode", "claude", "codex"],
                "providers": ["ollama", "anthropic", "openai"],
                "prefer": ["opencode"]
            },
            {
                "id": "local-medium-risk", "cheap": true,
                "classes": identity(&["standard-low", "standard-medium", "standard-high"]),
                "requires": ["narrow", "test_verified"], "max_risk": "medium",
                "harnesses": ["claude", "codex", "opencode"],
                "providers": ["anthropic", "openai", "ollama"]
            },
            {"id": "design", "classes": all, "harnesses": claude_or_codex},
            {"id": "planning", "classes": all, "harnesses": claude_or_codex},
            {"id": "deep-analysis", "classes": all},
            {"id": "art", "classes": all, "harnesses": ["codex"], "hold": true},
            {"id": "repair", "classes": all, "providers": ["anthropic", "openai", "google"]}
        ],
        "reserved": [
            {"classes": ["deep-low", "deep-high"], "harness": "claude", "only_lanes": ["design", "deep-analysis"]}
        ],
        "classifier_instructions": CLASSIFIER_INSTRUCTIONS,
    });
    let policy = serde_json::from_value(policy).expect("the reference policy parses");
    (policy, options())
}

/// Live-shaped options: Claude, Codex and Gemini on hosted providers; local
/// models through OpenCode at the Simple and Standard levels.
pub fn options() -> Options {
    let offer = |harness: &str| {
        let provider = match harness {
            "claude" => "anthropic",
            "codex" => "openai",
            "gemini" => "google",
            "opencode" => "ollama",
            other => panic!("no provider for {other}"),
        };
        Offer {
            provider: provider.into(),
            harness: harness.into(),
        }
    };
    let harnesses = |class: &str| -> &[&str] {
        match class {
            "standard-medium" => &["claude", "codex", "opencode"],
            "complex-low" | "complex-high" => &["claude", "codex", "gemini"],
            "deep-low" | "deep-high" => &["claude", "codex"],
            _ => &["claude", "codex", "gemini", "opencode"],
        }
    };
    let classes: BTreeMap<Class, Vec<Offer>> = ASSIGNED_CLASSES
        .iter()
        .map(|c| {
            let class = c.parse().expect("an assigned class parses");
            (class, harnesses(c).iter().map(|h| offer(h)).collect())
        })
        .collect();
    Options { classes }
}

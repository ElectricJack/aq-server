//! The compiled routing policy (spec 0011 §2, §6). A model compiles the
//! owner's prose into a [`Policy`]; [`validate`] is the deterministic gate that
//! decides whether it may take effect, [`digest`] names it on every route, and
//! [`summary`] shows it back to the owner in their own words.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::classify::{Flag, Options, Risk};
use crate::{Class, Parent};

/// The only policy format version this crate reads.
pub const POLICY_VERSION: u32 = 2;
/// Largest size of `classifier_instructions`, in bytes.
pub const CLASSIFIER_INSTRUCTIONS_MAX_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub version: u32,
    /// The kind used when neither the task nor the classification names a known one.
    pub default_kind: String,
    pub kinds: BTreeMap<String, KindRule>,
    #[serde(default)]
    pub origins: BTreeMap<String, OriginRule>,
    pub risk: BTreeMap<Risk, RiskRule>,
    #[serde(default)]
    pub lanes: Vec<Lane>,
    #[serde(default)]
    pub reserved: Vec<Reserved>,
    pub classifier_instructions: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KindRule {
    /// The level the kind starts from.
    pub class: Class,
    /// The highest level a requested level is clamped to.
    pub max_class: Class,
    pub lane: Option<String>,
    /// Whether work of this kind may take a cheap lane.
    #[serde(default)]
    pub narrow: bool,
    #[serde(default)]
    pub prefer_harnesses: Vec<String>,
    pub escalate: Option<Escalate>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Escalate {
    pub after_failures: u32,
    pub to_class: Class,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginRule {
    /// `Some(false)` keeps tasks of this origin off cheap lanes.
    pub narrow: Option<bool>,
    /// The lane for tasks of this origin whose kind has none.
    pub lane: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiskRule {
    /// The lowest level work at this risk may run at.
    pub min_class: Class,
    /// The only harnesses that may take work at this risk; empty is no restriction.
    pub harnesses: Vec<String>,
    pub relax: Option<Relax>,
}

/// A lower floor that applies when every flag in `requires` is set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Relax {
    pub class: Class,
    pub requires: Vec<Flag>,
}

/// A named set of restrictions. A kind or origin lane applies `class` (when
/// set), its allowlists, `prefer` and `hold`. A `cheap` lane is taken by
/// narrow work whose current level is a key of `classes`, and moves it to the
/// mapped level; an identity entry keeps the level. Only cheap lanes have
/// `classes`. A lane named by a reservation may have nothing else.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lane {
    pub id: String,
    pub class: Option<Class>,
    #[serde(default)]
    pub classes: BTreeMap<Class, Class>,
    #[serde(default)]
    pub harnesses: Vec<String>,
    #[serde(default)]
    pub providers: Vec<String>,
    /// Harnesses tried first.
    #[serde(default)]
    pub prefer: Vec<String>,
    /// Wait for the allowed harnesses rather than going elsewhere.
    #[serde(default)]
    pub hold: bool,
    #[serde(default)]
    pub requires: Vec<Flag>,
    pub max_risk: Option<Risk>,
    #[serde(default)]
    pub cheap: bool,
}

/// `harness` is removed at `classes` unless the route's lane is in `only_lanes`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reserved {
    pub classes: Vec<Class>,
    pub harness: String,
    pub only_lanes: Vec<String>,
}

/// One reason a compiled policy may not take effect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    /// Where in the compiled policy, for example `kinds.bugfix.escalate.to_class`.
    pub path: String,
    pub message: String,
}

/// Every reason `policy` may not take effect with what `options` offers.
/// Empty means it may.
pub fn validate(policy: &Policy, options: &Options) -> Vec<Problem> {
    let mut v = Validator {
        options,
        harnesses: options.offers().map(|o| o.harness.as_str()).collect(),
        providers: options.offers().map(|o| o.provider.as_str()).collect(),
        lanes: policy.lanes.iter().map(|l| l.id.as_str()).collect(),
        problems: Vec::new(),
    };
    if policy.version != POLICY_VERSION {
        v.push(
            "version",
            format!(
                "policy version {} is not supported; expected {POLICY_VERSION}",
                policy.version
            ),
        );
    }
    if !policy.kinds.contains_key(&policy.default_kind) {
        v.push(
            "default_kind",
            format!(
                "the default kind of work {:?} is not one of the kinds",
                policy.default_kind
            ),
        );
    }
    for (id, kind) in &policy.kinds {
        let at = format!("kinds.{id}");
        v.class(&format!("{at}.class"), kind.class);
        v.class(&format!("{at}.max_class"), kind.max_class);
        if kind.class > kind.max_class {
            v.push(
                &format!("{at}.class"),
                format!(
                    "{} is above the highest level for this kind, {}",
                    class_words(kind.class),
                    class_words(kind.max_class)
                ),
            );
        }
        v.lane_ref(&format!("{at}.lane"), kind.lane.as_deref());
        for (i, h) in kind.prefer_harnesses.iter().enumerate() {
            v.harness(&format!("{at}.prefer_harnesses[{i}]"), h);
        }
        if let Some(escalate) = &kind.escalate {
            v.class(&format!("{at}.escalate.to_class"), escalate.to_class);
        }
    }
    for (id, origin) in &policy.origins {
        v.lane_ref(&format!("origins.{id}.lane"), origin.lane.as_deref());
    }
    if !policy.risk.contains_key(&Risk::Medium) {
        v.push(
            "risk.medium",
            "there is no rule for medium risk, the risk assumed when a task cannot be classified"
                .into(),
        );
    }
    for (risk, rule) in &policy.risk {
        let at = format!("risk.{}", risk.as_str());
        v.class(&format!("{at}.min_class"), rule.min_class);
        for (i, h) in rule.harnesses.iter().enumerate() {
            v.harness(&format!("{at}.harnesses[{i}]"), h);
        }
        if let Some(relax) = &rule.relax {
            v.class(&format!("{at}.relax.class"), relax.class);
            if relax.class > rule.min_class {
                v.push(
                    &format!("{at}.relax.class"),
                    format!(
                        "the relaxed level {} is above the minimum level {}",
                        class_words(relax.class),
                        class_words(rule.min_class)
                    ),
                );
            }
        }
    }
    let mut seen = BTreeSet::new();
    for (i, lane) in policy.lanes.iter().enumerate() {
        let at = format!("lanes[{i}]");
        if !seen.insert(lane.id.as_str()) {
            v.push(
                &format!("{at}.id"),
                format!("lane {:?} is defined more than once", lane.id),
            );
        }
        if lane.cheap && lane.classes.is_empty() {
            v.push(
                &format!("{at}.classes"),
                format!("cheaper lane {:?} names no levels it takes", lane.id),
            );
        }
        if !lane.cheap && !lane.classes.is_empty() {
            v.push(
                &format!("{at}.classes"),
                format!(
                    "lane {:?} is not a cheaper lane, so its level map would never be read",
                    lane.id
                ),
            );
        }
        let reserved_for = policy
            .reserved
            .iter()
            .any(|r| r.only_lanes.contains(&lane.id));
        let restricts = lane.class.is_some()
            || !lane.harnesses.is_empty()
            || !lane.providers.is_empty()
            || !lane.prefer.is_empty()
            || lane.hold;
        if !lane.cheap && !restricts && !reserved_for {
            v.push(&at, format!("lane {:?} has no effect", lane.id));
        }
        if let Some(class) = lane.class {
            v.class(&format!("{at}.class"), class);
        }
        for (from, to) in &lane.classes {
            v.class(&format!("{at}.classes.{from}"), *from);
            v.class(&format!("{at}.classes.{from}"), *to);
        }
        for (i, h) in lane.harnesses.iter().enumerate() {
            v.harness(&format!("{at}.harnesses[{i}]"), h);
        }
        for (i, p) in lane.providers.iter().enumerate() {
            v.provider(&format!("{at}.providers[{i}]"), p);
        }
        for (i, h) in lane.prefer.iter().enumerate() {
            v.harness(&format!("{at}.prefer[{i}]"), h);
        }
    }
    for (i, reserved) in policy.reserved.iter().enumerate() {
        let at = format!("reserved[{i}]");
        for (j, class) in reserved.classes.iter().enumerate() {
            v.class(&format!("{at}.classes[{j}]"), *class);
        }
        v.harness(&format!("{at}.harness"), &reserved.harness);
        for (j, lane) in reserved.only_lanes.iter().enumerate() {
            v.lane_ref(&format!("{at}.only_lanes[{j}]"), Some(lane));
        }
    }
    let instructions = &policy.classifier_instructions;
    if instructions.trim().is_empty() {
        v.push(
            "classifier_instructions",
            "the classifier instructions are empty".into(),
        );
    } else if instructions.len() > CLASSIFIER_INSTRUCTIONS_MAX_BYTES {
        v.push(
            "classifier_instructions",
            format!(
                "the classifier instructions are {} bytes, over the {CLASSIFIER_INSTRUCTIONS_MAX_BYTES} byte limit",
                instructions.len()
            ),
        );
    }
    v.problems
}

struct Validator<'a> {
    options: &'a Options,
    harnesses: BTreeSet<&'a str>,
    providers: BTreeSet<&'a str>,
    lanes: BTreeSet<&'a str>,
    problems: Vec<Problem>,
}

impl Validator<'_> {
    fn push(&mut self, path: &str, message: String) {
        self.problems.push(Problem {
            path: path.into(),
            message,
        });
    }

    fn class(&mut self, path: &str, class: Class) {
        if !self.options.classes.contains_key(&class) {
            self.push(
                path,
                format!("{} has no assigned profile in Settings", class_words(class)),
            );
        }
    }

    fn harness(&mut self, path: &str, harness: &str) {
        if !self.harnesses.contains(harness) {
            self.push(
                path,
                format!("harness {harness:?} is not offered by any assigned profile"),
            );
        }
    }

    fn provider(&mut self, path: &str, provider: &str) {
        if !self.providers.contains(provider) {
            self.push(
                path,
                format!("provider {provider:?} is not offered by any assigned profile"),
            );
        }
    }

    fn lane_ref(&mut self, path: &str, lane: Option<&str>) {
        if let Some(lane) = lane
            && !self.lanes.contains(lane)
        {
            self.push(path, format!("there is no lane {lane:?}"));
        }
    }
}

/// `"sha256:"` and the hex SHA-256 of the policy's canonical JSON (object keys
/// sorted, no whitespace).
pub fn digest(policy: &Policy) -> String {
    let value = serde_json::to_value(policy).expect("a policy serializes to JSON");
    let mut canonical = String::new();
    write_canonical(&value, &mut canonical);
    let mut out = String::from("sha256:");
    for byte in Sha256::digest(canonical.as_bytes()) {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (key, value)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(value, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// The policy in plain sentences for the owner: one per kind, risk level and
/// lane, then the default kind, origins and reservations. No identifiers.
pub fn summary(policy: &Policy) -> Vec<String> {
    let mut out = Vec::new();
    for (id, kind) in &policy.kinds {
        out.push(kind_sentence(id, kind));
    }
    if policy.kinds.contains_key(&policy.default_kind) {
        out.push(format!(
            "Work of any other kind, or of a kind that can't be told, is handled as {}.",
            kind_words(&policy.default_kind)
        ));
    }
    let narrow: Vec<String> = policy
        .kinds
        .iter()
        .filter(|(_, k)| k.narrow)
        .map(|(id, _)| kind_words(id))
        .collect();
    if policy.lanes.iter().any(|l| l.cheap) && !narrow.is_empty() {
        out.push(format!("Cheaper lanes are open to {}.", and_list(&narrow)));
    }
    for risk in Risk::ALL {
        out.push(risk_sentence(risk, policy.risk.get(&risk)));
    }
    out.extend(policy.lanes.iter().filter_map(lane_sentence));
    for (id, origin) in &policy.origins {
        out.push(origin_sentence(id, origin));
    }
    for reserved in &policy.reserved {
        out.push(reserved_sentence(reserved));
    }
    out
}

fn kind_sentence(id: &str, kind: &KindRule) -> String {
    let mut clauses = vec![class_words(kind.class)];
    match &kind.escalate {
        Some(e) => clauses.push(format!(
            "{} after {} failed attempt{}",
            class_words(e.to_class),
            e.after_failures,
            if e.after_failures == 1 { "" } else { "s" }
        )),
        None if kind.max_class > kind.class => clauses.push(format!(
            "up to {} when the task asks for it",
            class_words(kind.max_class)
        )),
        None => {}
    }
    if let Some(lane) = &kind.lane {
        clauses.push(format!("through the {} lane", lane_words(lane)));
    }
    if !kind.prefer_harnesses.is_empty() {
        clauses.push(format!(
            "{} first",
            or_list(&harness_list(&kind.prefer_harnesses))
        ));
    }
    format!("{}: {}.", kind_words(id), clauses.join("; "))
}

fn risk_sentence(risk: Risk, rule: Option<&RiskRule>) -> String {
    let Some(rule) = rule else {
        return format!(
            "{}: no minimum level; any offered harness may take it.",
            risk_words(risk)
        );
    };
    let mut floor = format!("at least {}", class_words(rule.min_class));
    if let Some(relax) = &rule.relax {
        let _ = write!(floor, ", or {}", class_words(relax.class));
        if !relax.requires.is_empty() {
            let _ = write!(floor, " when {}", flags_words(&relax.requires));
        }
    }
    let mut clauses = vec![floor];
    if !rule.harnesses.is_empty() {
        clauses.push(format!("{} only", or_list(&harness_list(&rule.harnesses))));
    }
    format!("{}: {}.", risk_words(risk), clauses.join("; "))
}

/// `None` when the lane restricts nothing.
fn lane_sentence(lane: &Lane) -> Option<String> {
    let mut clauses = Vec::new();
    let mut scope = String::from(if lane.cheap { "cheaper lane" } else { "" });
    if !lane.requires.is_empty() || lane.max_risk.is_some() {
        if !scope.is_empty() {
            scope.push(' ');
        }
        scope.push_str("for work");
        if !lane.requires.is_empty() {
            let _ = write!(scope, " that is {}", flags_words(&lane.requires));
        }
        if let Some(max) = lane.max_risk {
            let _ = write!(scope, ", up to {}", risk_words(max).to_lowercase());
        }
    }
    if lane.cheap && !lane.classes.is_empty() {
        let _ = write!(
            scope,
            "{}at {}",
            if scope.is_empty() { "" } else { ", " },
            class_list(lane.classes.keys().copied())
        );
    }
    if !scope.is_empty() {
        clauses.push(scope);
    }
    if let Some(class) = lane.class {
        clauses.push(format!("sets {}", class_words(class)));
    }
    for (from, to) in &lane.classes {
        if from != to {
            clauses.push(format!(
                "{} becomes {}",
                class_words(*from),
                class_words(*to)
            ));
        }
    }
    let mut who = Vec::new();
    if !lane.harnesses.is_empty() {
        who.push(format!(
            "taken by {}",
            or_list(&harness_list(&lane.harnesses))
        ));
    }
    if !lane.providers.is_empty() {
        let providers: Vec<String> = lane.providers.iter().map(|p| provider_words(p)).collect();
        who.push(format!("using {}", or_list(&providers)));
    }
    if !who.is_empty() {
        clauses.push(who.join(", "));
    }
    if !lane.prefer.is_empty() {
        clauses.push(format!("{} first", or_list(&harness_list(&lane.prefer))));
    }
    if lane.hold {
        let them = if lane.harnesses.len() == 1 {
            "it"
        } else {
            "them"
        };
        clauses.push(format!("waits for {them} rather than going elsewhere"));
    }
    if clauses.is_empty() {
        return None;
    }
    Some(format!(
        "{} lane: {}.",
        lane_words(&lane.id),
        clauses.join("; ")
    ))
}

fn origin_sentence(id: &str, origin: &OriginRule) -> String {
    let mut clauses = Vec::new();
    match origin.narrow {
        Some(false) => clauses.push("never a cheaper lane".to_string()),
        Some(true) => clauses.push("may take a cheaper lane".to_string()),
        None => {}
    }
    if let Some(lane) = &origin.lane {
        clauses.push(format!("through the {} lane", lane_words(lane)));
    }
    if clauses.is_empty() {
        clauses.push("no special handling".into());
    }
    format!("{}: {}.", origin_words(id), clauses.join("; "))
}

fn reserved_sentence(reserved: &Reserved) -> String {
    let lanes: Vec<String> = reserved.only_lanes.iter().map(|l| lane_words(l)).collect();
    let used = match lanes.len() {
        0 => "is never used".to_string(),
        1 => format!("is kept for the {} lane", lanes[0]),
        _ => format!("is kept for the {} lanes", or_list(&lanes)),
    };
    format!(
        "{} at {} {used}.",
        harness_words(&reserved.harness),
        class_list(reserved.classes.iter().copied())
    )
}

fn parent_words(parent: Parent) -> &'static str {
    match parent {
        Parent::Simple => "Simple",
        Parent::Standard => "Standard",
        Parent::Complex => "Complex",
        Parent::Deep => "Deep",
    }
}

/// `Standard, high effort`.
pub(crate) fn class_words(class: Class) -> String {
    format!(
        "{}, {} effort",
        parent_words(class.parent),
        class.effort.as_str()
    )
}

/// `Simple (low or high effort) or Deep (high effort)`.
fn class_list(classes: impl Iterator<Item = Class>) -> String {
    let mut by_parent: BTreeMap<Parent, BTreeSet<crate::Effort>> = BTreeMap::new();
    for class in classes {
        by_parent
            .entry(class.parent)
            .or_default()
            .insert(class.effort);
    }
    let groups: Vec<String> = by_parent
        .into_iter()
        .map(|(parent, efforts)| {
            let efforts: Vec<String> = efforts.iter().map(|e| e.as_str().to_string()).collect();
            format!("{} ({} effort)", parent_words(parent), or_list(&efforts))
        })
        .collect();
    or_list(&groups)
}

pub(crate) fn risk_words(risk: Risk) -> &'static str {
    match risk {
        Risk::Low => "Low risk",
        Risk::Medium => "Medium risk",
        Risk::High => "High risk",
        Risk::VeryHigh => "Very high risk",
    }
}

pub(crate) fn flags_words(flags: &[Flag]) -> String {
    let words: Vec<String> = flags
        .iter()
        .map(|f| {
            match f {
                Flag::Narrow => "narrow",
                Flag::TestVerified => "covered by a test",
                Flag::DeepReasoning => "in need of advanced reasoning",
            }
            .to_string()
        })
        .collect();
    and_list(&words)
}

pub(crate) fn harness_list(ids: &[String]) -> Vec<String> {
    ids.iter().map(|h| harness_words(h)).collect()
}

pub(crate) fn harness_words(id: &str) -> String {
    match id {
        "claude" => "Claude".into(),
        "codex" => "Codex".into(),
        "gemini" => "Gemini".into(),
        "opencode" => "OpenCode".into(),
        other => humanize(other),
    }
}

pub(crate) fn provider_words(id: &str) -> String {
    match id {
        "anthropic" => "Anthropic".into(),
        "openai" => "OpenAI".into(),
        "google" => "Google".into(),
        "ollama" => "local models".into(),
        other => humanize(other),
    }
}

pub(crate) fn kind_words(id: &str) -> String {
    match id {
        "feature" => "Features".into(),
        "bugfix" => "Bug fixes".into(),
        "refactor" => "Refactors".into(),
        "test" => "Tests".into(),
        "docs" => "Docs".into(),
        "chore" => "Chores".into(),
        "sync" => "Syncs".into(),
        "spec" => "Specs".into(),
        "design" => "Designs".into(),
        "plan" => "Implementation plans".into(),
        "review" => "Reviews of someone else's work".into(),
        "research" => "Research".into(),
        "analysis" => "Analysis".into(),
        "art" => "Art and visual design".into(),
        other => humanize(other),
    }
}

pub(crate) fn origin_words(id: &str) -> String {
    match id {
        "repair" => "Repairs to a failed integration or build".into(),
        other => humanize(other),
    }
}

pub(crate) fn lane_words(id: &str) -> String {
    humanize(id)
}

/// `free-hosted_models` to `Free hosted models`.
fn humanize(id: &str) -> String {
    let spaced = id.replace(['-', '_'], " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => spaced,
    }
}

fn joined(items: &[String], last: &str) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., tail] => format!("{} {last} {tail}", init.join(", ")),
    }
}

pub(crate) fn or_list(items: &[String]) -> String {
    joined(items, "or")
}

fn and_list(items: &[String]) -> String {
    joined(items, "and")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    fn class(s: &str) -> Class {
        s.parse().unwrap()
    }

    fn lane_index(policy: &Policy, id: &str) -> usize {
        policy.lanes.iter().position(|l| l.id == id).unwrap()
    }

    fn paths(problems: &[Problem]) -> Vec<&str> {
        problems.iter().map(|p| p.path.as_str()).collect()
    }

    #[test]
    fn the_reference_policy_validates_against_the_live_shaped_options() {
        let (policy, options) = fixtures::reference();
        assert_eq!(validate(&policy, &options), vec![]);
        assert_eq!(policy.version, POLICY_VERSION);
        assert_eq!(policy.risk[&Risk::Medium].harnesses, ["claude", "codex"]);
        assert!(!policy.risk.contains_key(&Risk::Low));
        for kind in ["feature", "bugfix", "refactor", "test", "docs", "research"] {
            assert_eq!(policy.kinds[kind].class, class("standard-high"), "{kind}");
            assert!(policy.kinds[kind].prefer_harnesses.is_empty(), "{kind}");
        }
        assert_eq!(policy.kinds["design"].prefer_harnesses, ["claude"]);
        assert_eq!(policy.kinds["plan"].prefer_harnesses, ["claude"]);
        let art = &policy.lanes[lane_index(&policy, "art")];
        assert_eq!(art.harnesses, ["codex"]);
        assert!(art.hold);
        assert_eq!(
            policy.reserved[0].classes,
            [class("deep-low"), class("deep-high")]
        );
        assert_eq!(policy.reserved[0].harness, "claude");
        let repair = &policy.lanes[lane_index(&policy, "repair")];
        assert!(!repair.providers.iter().any(|p| p == "ollama"));
        assert_eq!(policy.origins["repair"].narrow, Some(false));
        // Low risk: local models first, nobody shut out.
        let local_first = &policy.lanes[lane_index(&policy, "local-first")];
        assert!(local_first.cheap && local_first.harnesses.is_empty());
        assert!(local_first.providers.is_empty());
        assert_eq!(local_first.prefer, ["opencode"]);
        // Medium risk: Claude, Codex or a local model, no preference.
        let medium = &policy.lanes[lane_index(&policy, "local-medium-risk")];
        assert_eq!(medium.max_risk, Some(Risk::Medium));
        assert_eq!(medium.harnesses, ["claude", "codex", "opencode"]);
        assert_eq!(medium.providers, ["anthropic", "openai", "ollama"]);
        assert!(medium.prefer.is_empty());
        for lane in policy.lanes.iter().filter(|l| !l.cheap) {
            assert!(lane.classes.is_empty(), "{}", lane.id);
            assert!(!lane.providers.iter().any(|p| p == "ollama"), "{}", lane.id);
        }
        assert_eq!(policy.kinds["spec"].lane.as_deref(), Some("design"));
        let back: Policy = serde_json::from_value(serde_json::to_value(&policy).unwrap()).unwrap();
        assert_eq!(back, policy);
    }

    #[test]
    fn each_problem_kind_is_reported_with_its_path() {
        type Mutation = fn(&mut Policy);
        let rows: Vec<(&str, Mutation, String)> = vec![
            ("version", |p| p.version = 1, "version".into()),
            (
                "cheap lane with no levels",
                |p| {
                    let i = lane_index(p, "local-first");
                    p.lanes[i].classes.clear();
                },
                format!(
                    "lanes[{}].classes",
                    lane_index(&fixtures::reference().0, "local-first")
                ),
            ),
            (
                "level map on a lane that is not cheap",
                |p| {
                    let i = lane_index(p, "art");
                    p.lanes[i]
                        .classes
                        .insert(class("standard-high"), class("standard-high"));
                },
                format!(
                    "lanes[{}].classes",
                    lane_index(&fixtures::reference().0, "art")
                ),
            ),
            (
                "lane with no effect",
                |p| {
                    let i = lane_index(p, "art");
                    p.lanes[i].harnesses.clear();
                    p.lanes[i].hold = false;
                },
                format!("lanes[{}]", lane_index(&fixtures::reference().0, "art")),
            ),
            (
                "lane no longer named by a reservation has no effect",
                |p| p.reserved[0].only_lanes.retain(|l| l != "deep-analysis"),
                format!(
                    "lanes[{}]",
                    lane_index(&fixtures::reference().0, "deep-analysis")
                ),
            ),
            (
                "default kind",
                |p| p.default_kind = "unknown".into(),
                "default_kind".into(),
            ),
            (
                "kind class unassigned",
                |p| p.kinds.get_mut("chore").unwrap().class = class("simple-medium"),
                "kinds.chore.class".into(),
            ),
            (
                "kind max class unassigned",
                |p| p.kinds.get_mut("research").unwrap().max_class = class("complex-max"),
                "kinds.research.max_class".into(),
            ),
            (
                "escalation class unassigned",
                |p| {
                    let rule = p.kinds.get_mut("bugfix").unwrap();
                    rule.escalate.as_mut().unwrap().to_class = class("complex-max");
                },
                "kinds.bugfix.escalate.to_class".into(),
            ),
            (
                "class above max class",
                |p| p.kinds.get_mut("docs").unwrap().max_class = class("standard-medium"),
                "kinds.docs.class".into(),
            ),
            (
                "kind lane unknown",
                |p| p.kinds.get_mut("art").unwrap().lane = Some("nowhere".into()),
                "kinds.art.lane".into(),
            ),
            (
                "origin lane unknown",
                |p| p.origins.get_mut("repair").unwrap().lane = Some("nowhere".into()),
                "origins.repair.lane".into(),
            ),
            (
                "reserved lane unknown",
                |p| p.reserved[0].only_lanes[0] = "nowhere".into(),
                "reserved[0].only_lanes[0]".into(),
            ),
            (
                "duplicate lane",
                |p| p.lanes.push(p.lanes[0].clone()),
                format!("lanes[{}].id", fixtures::reference().0.lanes.len()),
            ),
            (
                "kind preferred harness not offered",
                |p| p.kinds.get_mut("design").unwrap().prefer_harnesses[0] = "cursor".into(),
                "kinds.design.prefer_harnesses[0]".into(),
            ),
            (
                "risk harness not offered",
                |p| {
                    let rule = p.risk.get_mut(&Risk::High).unwrap();
                    rule.harnesses.push("copilot".into());
                },
                "risk.high.harnesses[2]".into(),
            ),
            (
                "risk min class unassigned",
                |p| p.risk.get_mut(&Risk::High).unwrap().min_class = class("standard-max"),
                "risk.high.min_class".into(),
            ),
            (
                "relax class above min class",
                |p| {
                    let rule = p.risk.get_mut(&Risk::VeryHigh).unwrap();
                    rule.relax.as_mut().unwrap().class = class("complex-high");
                },
                "risk.very_high.relax.class".into(),
            ),
            (
                "no medium risk rule",
                |p| {
                    p.risk.remove(&Risk::Medium);
                },
                "risk.medium".into(),
            ),
            (
                "lane provider not offered",
                |p| {
                    let i = lane_index(p, "local-medium-risk");
                    p.lanes[i].providers[0] = "groq".into();
                },
                format!(
                    "lanes[{}].providers[0]",
                    lane_index(&fixtures::reference().0, "local-medium-risk")
                ),
            ),
            (
                "lane harness not offered",
                |p| {
                    let i = lane_index(p, "art");
                    p.lanes[i].harnesses[0] = "cursor".into();
                },
                format!(
                    "lanes[{}].harnesses[0]",
                    lane_index(&fixtures::reference().0, "art")
                ),
            ),
            (
                "lane preferred harness not offered",
                |p| {
                    let i = lane_index(p, "local-first");
                    p.lanes[i].prefer[0] = "cursor".into();
                },
                format!(
                    "lanes[{}].prefer[0]",
                    lane_index(&fixtures::reference().0, "local-first")
                ),
            ),
            (
                "lane class unassigned",
                |p| {
                    let i = lane_index(p, "art");
                    p.lanes[i].class = Some(class("standard-max"));
                },
                format!(
                    "lanes[{}].class",
                    lane_index(&fixtures::reference().0, "art")
                ),
            ),
            (
                "lane class map key unassigned",
                |p| {
                    let i = lane_index(p, "local-first");
                    p.lanes[i]
                        .classes
                        .insert(class("standard-max"), class("standard-high"));
                },
                format!(
                    "lanes[{}].classes.standard-max",
                    lane_index(&fixtures::reference().0, "local-first")
                ),
            ),
            (
                "lane class map value unassigned",
                |p| {
                    let i = lane_index(p, "local-first");
                    p.lanes[i]
                        .classes
                        .insert(class("simple-low"), class("simple-medium"));
                },
                format!(
                    "lanes[{}].classes.simple-low",
                    lane_index(&fixtures::reference().0, "local-first")
                ),
            ),
            (
                "reserved class unassigned",
                |p| p.reserved[0].classes.push(class("deep-max")),
                "reserved[0].classes[2]".into(),
            ),
            (
                "reserved harness not offered",
                |p| p.reserved[0].harness = "cursor".into(),
                "reserved[0].harness".into(),
            ),
            (
                "empty classifier instructions",
                |p| p.classifier_instructions = " \n".into(),
                "classifier_instructions".into(),
            ),
            (
                "oversized classifier instructions",
                |p| p.classifier_instructions = "x".repeat(CLASSIFIER_INSTRUCTIONS_MAX_BYTES + 1),
                "classifier_instructions".into(),
            ),
        ];
        for (name, mutate, path) in rows {
            let (mut policy, options) = fixtures::reference();
            mutate(&mut policy);
            let problems = validate(&policy, &options);
            assert_eq!(paths(&problems), [path.as_str()], "{name}: {problems:?}");
            assert!(!problems[0].message.is_empty(), "{name}");
        }
        let (mut policy, options) = fixtures::reference();
        policy.classifier_instructions = "x".repeat(CLASSIFIER_INSTRUCTIONS_MAX_BYTES);
        assert_eq!(validate(&policy, &options), vec![]);
    }

    #[test]
    fn a_harness_settings_stops_offering_is_reported_wherever_the_policy_names_it() {
        let (policy, mut options) = fixtures::reference();
        for offers in options.classes.values_mut() {
            offers.retain(|offer| offer.harness != "codex");
        }
        let problems = validate(&policy, &options);
        let art = lane_index(&policy, "art");
        let expected = [
            "risk.high.harnesses[1]".to_string(),
            "risk.medium.harnesses[1]".to_string(),
            "risk.very_high.harnesses[1]".to_string(),
            format!("lanes[{art}].harnesses[0]"),
        ];
        for path in &expected {
            assert!(
                paths(&problems).contains(&path.as_str()),
                "{path}: {problems:?}"
            );
        }
        // Codex was the only harness for the provider "openai", so it goes too.
        for problem in &problems {
            assert!(
                [
                    "harness \"codex\" is not offered by any assigned profile",
                    "provider \"openai\" is not offered by any assigned profile"
                ]
                .contains(&problem.message.as_str()),
                "{problem:?}"
            );
        }
    }

    #[test]
    fn problem_messages_name_the_level_in_words() {
        let (mut policy, options) = fixtures::reference();
        policy
            .kinds
            .get_mut("bugfix")
            .unwrap()
            .escalate
            .as_mut()
            .unwrap()
            .to_class = class("complex-max");
        let problems = validate(&policy, &options);
        assert_eq!(
            problems[0].message,
            "Complex, max effort has no assigned profile in Settings"
        );
    }

    /// Writes `value` as JSON with every object's keys in reverse order.
    fn reversed(value: &Value) -> String {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                keys.reverse();
                let entries: Vec<String> = keys
                    .into_iter()
                    .map(|k| format!("{}:{}", Value::String(k.clone()), reversed(&map[k])))
                    .collect();
                format!("{{{}}}", entries.join(","))
            }
            Value::Array(items) => {
                let items: Vec<String> = items.iter().map(reversed).collect();
                format!("[{}]", items.join(","))
            }
            other => other.to_string(),
        }
    }

    #[test]
    fn digest_is_stable_across_key_order_and_changes_with_content() {
        let (policy, _) = fixtures::reference();
        let d = digest(&policy);
        let hex = d.strip_prefix("sha256:").unwrap();
        assert_eq!(hex.len(), 64);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(digest(&policy), d);

        let text = reversed(&serde_json::to_value(&policy).unwrap());
        assert!(text.starts_with("{\"version\":2,"));
        let reordered: Policy = serde_json::from_str(&text).unwrap();
        assert_eq!(digest(&reordered), d);

        let mut changed = policy.clone();
        changed.risk.get_mut(&Risk::High).unwrap().harnesses.pop();
        assert_ne!(digest(&changed), d);
        let mut changed = policy.clone();
        changed.classifier_instructions.push('.');
        assert_ne!(digest(&changed), d);
        let mut changed = policy;
        changed.lanes.swap(0, 1);
        assert_ne!(digest(&changed), d);
    }

    #[test]
    fn summary_reads_in_plain_words_and_contains_no_identifier_with_a_dash_or_underscore() {
        let (policy, _) = fixtures::reference();
        let sentences = summary(&policy);
        for s in &sentences {
            assert!(s.ends_with('.'), "{s}");
            assert!(!s.contains('-') && !s.contains('_'), "{s}");
            assert!(s.chars().next().unwrap().is_uppercase(), "{s}");
        }
        let has = |expected: &str| {
            assert!(
                sentences.iter().any(|s| s == expected),
                "missing {expected:?} in {sentences:#?}"
            )
        };
        has("Bug fixes: Standard, high effort; Complex, high effort after 1 failed attempt.");
        has("High risk: at least Standard, high effort; Claude or Codex only.");
        has(
            "Very high risk: at least Complex, low effort, or Standard, high effort when narrow and covered by a test; Claude or Codex only.",
        );
        has("Medium risk: at least Standard, high effort; Claude or Codex only.");
        has("Low risk: no minimum level; any offered harness may take it.");
        has(
            "Features: Standard, high effort; up to Complex, high effort when the task asks for it.",
        );
        has("Chores: Simple, high effort; up to Standard, high effort when the task asks for it.");
        has(
            "Designs: Complex, high effort; up to Deep, high effort when the task asks for it; through the Design lane; Claude first.",
        );
        has(
            "Art and visual design: Standard, high effort; up to Complex, high effort when the task asks for it; through the Art lane.",
        );
        has("Art lane: taken by Codex; waits for it rather than going elsewhere.");
        has("Repair lane: using Anthropic, OpenAI or Google.");
        has(
            "Repairs to a failed integration or build: never a cheaper lane; through the Repair lane.",
        );
        has(
            "Claude at Deep (low or high effort) is kept for the Design, Planning or Deep analysis lanes.",
        );
        has(
            "Local first lane: cheaper lane for work that is narrow and covered by a test, up to low risk, at Simple (low or high effort) or Standard (low, medium or high effort); OpenCode first.",
        );
        has(
            "Local medium risk lane: cheaper lane for work that is narrow and covered by a test, up to medium risk, at Standard (low, medium or high effort); taken by Claude, Codex or OpenCode, using Anthropic, OpenAI or local models.",
        );
        has(
            "Specs: Complex, high effort; up to Deep, high effort when the task asks for it; through the Design lane.",
        );
        has(
            "Cheaper lanes are open to Bug fixes, Chores, Docs, Features, Refactors, Research, Syncs and Tests.",
        );
        // The deep analysis lane only names who may use Deep Claude; it has no sentence of its own.
        assert!(
            !sentences
                .iter()
                .any(|s| s.starts_with("Deep analysis lane"))
        );
        has("Work of any other kind, or of a kind that can't be told, is handled as Features.");
        let kinds = policy.kinds.len();
        let lanes = policy.lanes.len() - 1;
        let others = 1 + 1 + policy.origins.len() + policy.reserved.len();
        assert_eq!(sentences.len(), kinds + Risk::ALL.len() + lanes + others);
    }

    #[test]
    fn summary_spells_unknown_ids_as_words() {
        assert_eq!(humanize("free-hosted_models"), "Free hosted models");
        assert_eq!(kind_words("security_audit"), "Security audit");
        assert_eq!(harness_words("cursor-agent"), "Cursor agent");
        assert_eq!(provider_words("ollama"), "local models");
        assert_eq!(
            class_list(
                [
                    class("simple-low"),
                    class("simple-high"),
                    class("deep-high")
                ]
                .into_iter()
            ),
            "Simple (low or high effort) or Deep (high effort)"
        );
    }

    #[test]
    fn a_policy_without_optional_sections_parses_and_unknown_fields_are_refused() {
        let minimal = serde_json::json!({
            "version": 2,
            "default_kind": "feature",
            "kinds": {"feature": {"class": "standard-high", "max_class": "standard-high"}},
            "risk": {"medium": {"min_class": "standard-high", "harnesses": []}},
            "classifier_instructions": "Classify the task."
        });
        let policy: Policy = serde_json::from_value(minimal.clone()).unwrap();
        assert!(policy.lanes.is_empty() && policy.origins.is_empty() && policy.reserved.is_empty());
        assert!(!policy.kinds["feature"].narrow);
        let mut extra = minimal.clone();
        extra["kinds"]["feature"]["weight"] = serde_json::json!(2);
        assert!(serde_json::from_value::<Policy>(extra).is_err());
        let mut extra = minimal;
        extra["routing"] = serde_json::json!(true);
        assert!(serde_json::from_value::<Policy>(extra).is_err());
    }
}

//! The planner (spec 0011 §3): applies a compiled routing policy to one task
//! and produces its route. Deterministic and pure; a model never produces a
//! route, so this function is the whole decision.

use crate::classify::{Classification, Flag, Options, Risk};
use crate::policy::{
    Lane, Policy, class_words, flags_words, harness_list, harness_words, kind_words, lane_words,
    or_list, origin_words, provider_words, risk_words,
};
use crate::{Class, Effort, Parent, ProviderIntent, Route, RouteReason, Source};

/// Where rule 6 sends a deep level the task does not need.
const COMPLEX_HIGH: Class = Class {
    parent: Parent::Complex,
    effort: Effort::High,
};

/// `NoCandidates` code: the route's class has no assigned profile.
pub const CLASS_UNASSIGNED: &str = "class_unassigned";
/// `NoCandidates` code: no offer for the route's class passes its allowlists.
pub const ALLOWLIST_EXCLUDES_ALL: &str = "allowlist_excludes_all";

/// What the task itself says: `aq:task_type`, `aq:intelligence_hint` (an
/// unparsable hint is `None`), `aq:origin`, and the charged failed attempts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskFacts {
    pub kind: Option<String>,
    pub class_hint: Option<Class>,
    pub origin: Option<String>,
    pub failed_attempts: u32,
}

/// The classifier's answer for the task.
#[derive(Clone, Debug, PartialEq)]
pub enum Classified {
    /// Not asked yet.
    Missing,
    /// Asked, and no usable answer came back.
    Failed,
    Answered(Classification),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    Planned(Route),
    /// Classify the task with these choices, then plan again.
    NeedsClassification {
        allowed_kinds: Vec<String>,
        allowed_classes: Vec<Class>,
    },
    /// Nothing allowed could ever take the task. `code` is
    /// [`CLASS_UNASSIGNED`] or [`ALLOWLIST_EXCLUDES_ALL`].
    NoCandidates {
        code: String,
        detail: String,
    },
}

/// An allowlist: `None` admits everyone, `Some` only its entries (and
/// `Some(vec![])` no one).
type Allow = Option<Vec<String>>;

/// A policy list as an allowlist: empty is no restriction.
fn allow(list: &[String]) -> Allow {
    (!list.is_empty()).then(|| list.to_vec())
}

fn admits(allow: &Allow, id: &str) -> bool {
    allow
        .as_ref()
        .is_none_or(|list| list.iter().any(|x| x == id))
}

/// `a` limited to what `b` admits, in `a`'s order.
fn intersect(a: Allow, b: Allow) -> Allow {
    match (a, b) {
        (Some(a), b) => Some(a.into_iter().filter(|x| admits(&b, x)).collect()),
        (None, b) => b,
    }
}

fn reason(reasons: &mut Vec<RouteReason>, code: &str, detail: String) {
    reasons.push(RouteReason {
        code: code.into(),
        detail,
    });
}

fn no_candidates(code: &str, detail: String) -> Plan {
    Plan::NoCandidates {
        code: code.into(),
        detail,
    }
}

/// The route for `task` under `policy`, whose digest and structure revision
/// are recorded on it. `policy` is one [`crate::policy::validate`] accepted.
/// The numbered comments are its steps, in order; each step that changes the
/// level or the allowlists records a reason on the route.
pub fn plan(
    policy: &Policy,
    policy_digest: &str,
    structure_revision: u64,
    options: &Options,
    task: &TaskFacts,
    classified: &Classified,
) -> Plan {
    // 1. Nothing to plan from yet.
    let answer = match classified {
        Classified::Missing => {
            return Plan::NeedsClassification {
                allowed_kinds: policy.kinds.keys().cloned().collect(),
                allowed_classes: options.classes.keys().copied().collect(),
            };
        }
        Classified::Failed => None,
        Classified::Answered(c) => Some(c),
    };
    let flag = |f: Flag| {
        answer.is_some_and(|c| match f {
            Flag::Narrow => c.narrow,
            Flag::TestVerified => c.test_verified,
            Flag::DeepReasoning => c.deep_reasoning,
        })
    };
    let all_set = |flags: &[Flag]| flags.iter().all(|f| flag(*f));
    let mut reasons = Vec::new();

    // 2. Kind.
    let kind_id = [task.kind.as_deref(), answer.map(|c| c.task_type.as_str())]
        .into_iter()
        .flatten()
        .find(|k| policy.kinds.contains_key(*k))
        .unwrap_or(&policy.default_kind);
    let Some(kind) = policy.kinds.get(kind_id) else {
        return no_candidates(
            CLASS_UNASSIGNED,
            format!(
                "no level: the default kind of work {kind_id:?} is not one of the policy's kinds"
            ),
        );
    };

    // 3. Risk.
    let risk = match answer {
        Some(c) => c.risk,
        None => {
            reason(
                &mut reasons,
                "classification_failed",
                "the task could not be classified".into(),
            );
            reason(
                &mut reasons,
                "risk_assumed",
                "the risk could not be judged, so medium risk is assumed".into(),
            );
            Risk::Medium
        }
    };

    // 4. Starting class; 5. clamp, for kinds that do not escalate.
    let mut escalated = false;
    let mut class = match &kind.escalate {
        Some(e) if task.failed_attempts >= e.after_failures => {
            escalated = true;
            let n = task.failed_attempts;
            reason(
                &mut reasons,
                "escalated_after_failure",
                format!(
                    "{n} charged failed attempt{}, and {} move up after {}: raised from {} to {}",
                    if n == 1 { "" } else { "s" },
                    kind_words(kind_id).to_lowercase(),
                    e.after_failures,
                    class_words(kind.class),
                    class_words(e.to_class)
                ),
            );
            e.to_class
        }
        Some(_) => kind.class,
        None => {
            let (start, from) = match (task.class_hint, answer) {
                (Some(hint), _) => (hint, "the task asked for"),
                (None, Some(c)) => (c.intelligence_class, "the classification answered"),
                (None, None) => (kind.class, "the kind of work starts at"),
            };
            if start > kind.max_class {
                reason(
                    &mut reasons,
                    "hint_clamped",
                    format!(
                        "{from} {}, above the highest level for {}: clamped to {}",
                        class_words(start),
                        kind_words(kind_id).to_lowercase(),
                        class_words(kind.max_class)
                    ),
                );
                kind.max_class
            } else {
                start
            }
        }
    };

    // 6. Deep only for advanced reasoning, here and wherever the class
    // changes below.
    let deep_reasoning = flag(Flag::DeepReasoning);
    class = keep_deep_for_reasoning(class, deep_reasoning, &mut reasons);

    // 7. Lane by kind, else by origin.
    let origin = task
        .origin
        .as_deref()
        .and_then(|id| policy.origins.get_key_value(id));
    let named = match (&kind.lane, origin) {
        (Some(lane), _) => Some((lane, kind_words(kind_id))),
        (None, Some((id, rule))) => rule.lane.as_ref().map(|lane| (lane, origin_words(id))),
        (None, None) => None,
    };
    let mut lane: Option<&Lane> = None;
    let mut harnesses: Allow = None;
    let mut providers: Allow = None;
    if let Some((id, who)) = named
        && let Some(matched) = policy.lanes.iter().find(|l| &l.id == id)
    {
        let mut detail = format!("{who}: through the {} lane", lane_words(id));
        if let Some(to) = matched.class
            && !escalated
            && to != class
        {
            detail += &format!(
                ", which sets {} in place of {}",
                class_words(to),
                class_words(class)
            );
            class = to;
        }
        reason(&mut reasons, "lane_matched", detail);
        class = keep_deep_for_reasoning(class, deep_reasoning, &mut reasons);
        harnesses = allow(&matched.harnesses);
        providers = allow(&matched.providers);
        lane = Some(matched);
    }

    // 8. Risk floor; 9. risk harnesses.
    let mut floor = None;
    if let Some(rule) = policy.risk.get(&risk) {
        let min = match &rule.relax {
            Some(relax) if all_set(&relax.requires) => {
                if class < rule.min_class {
                    reason(
                        &mut reasons,
                        "risk_relaxed",
                        format!(
                            "{} work that is {}: minimum lowered from {} to {}",
                            risk_words(risk),
                            flags_words(&relax.requires),
                            class_words(rule.min_class),
                            class_words(relax.class)
                        ),
                    );
                }
                relax.class
            }
            _ => rule.min_class,
        };
        // The Deep rule wins over a Deep floor, which `validate` refuses.
        floor = Some(deep_only_for_reasoning(min, deep_reasoning));
        if class < min {
            reason(
                &mut reasons,
                "class_raised_for_risk",
                format!(
                    "{}: raised from {} to {}",
                    risk_words(risk).to_lowercase(),
                    class_words(class),
                    class_words(min)
                ),
            );
            class = keep_deep_for_reasoning(min, deep_reasoning, &mut reasons);
        }
        harnesses = intersect(harnesses, allow(&rule.harnesses));
    }

    // 10. Cheap lanes, for answered narrow work with no lane yet. A lane with
    // no risk ceiling is closed, and one whose level would be below the risk
    // floor is passed over.
    let origin_closes = origin.is_some_and(|(_, rule)| rule.narrow == Some(false));
    if lane.is_none() && kind.narrow && !origin_closes && answer.is_some() {
        let cheap = policy.lanes.iter().filter(|l| l.cheap).find_map(|l| {
            if !all_set(&l.requires) || l.max_risk.is_none_or(|max| max < risk) {
                return None;
            }
            let to = *l.classes.get(&class)?;
            let lands = deep_only_for_reasoning(to, deep_reasoning);
            if floor.is_some_and(|min| lands < min) {
                return None;
            }
            let (h, p) = (allow(&l.harnesses), allow(&l.providers));
            options
                .classes
                .get(&lands)?
                .iter()
                .any(|o| admits(&h, &o.harness) && admits(&p, &o.provider))
                .then_some((l, to))
        });
        if let Some((matched, to)) = cheap {
            let mut detail = format!("{} work", risk_words(risk));
            if !matched.requires.is_empty() {
                detail += &format!(" that is {}", flags_words(&matched.requires));
            }
            detail += &format!(" takes the cheaper {} lane", lane_words(&matched.id));
            if to != class {
                detail += &format!(
                    ", which moves {} to {}",
                    class_words(class),
                    class_words(to)
                );
            }
            reason(&mut reasons, "lane_matched", detail);
            class = keep_deep_for_reasoning(to, deep_reasoning, &mut reasons);
            harnesses = allow(&matched.harnesses);
            providers = allow(&matched.providers);
            lane = Some(matched);
        }
    }

    // 11. Preference.
    let prefer = match lane {
        Some(l) if !l.prefer.is_empty() => l.prefer.clone(),
        _ => kind.prefer_harnesses.clone(),
    };

    // 12. Reserved harnesses.
    let lane_id = lane.map(|l| l.id.as_str());
    for reserved in &policy.reserved {
        let kept = lane_id.is_some_and(|id| reserved.only_lanes.iter().any(|l| l == id));
        if !reserved.classes.contains(&class) || kept {
            continue;
        }
        let current = harnesses
            .clone()
            .unwrap_or_else(|| offered_harnesses(options, class));
        if !current.contains(&reserved.harness) {
            continue;
        }
        let lanes: Vec<String> = reserved.only_lanes.iter().map(|l| lane_words(l)).collect();
        let kept_for = match lanes.len() {
            0 => "is never used".to_string(),
            1 => format!("is kept for the {} lane", lanes[0]),
            _ => format!("is kept for the {} lanes", or_list(&lanes)),
        };
        reason(
            &mut reasons,
            "reserved_harness_removed",
            format!(
                "{} at {} {kept_for}",
                harness_words(&reserved.harness),
                class_words(class)
            ),
        );
        harnesses = Some(
            current
                .into_iter()
                .filter(|h| *h != reserved.harness)
                .collect(),
        );
    }

    // 13. Candidates.
    let Some(offers) = options.classes.get(&class) else {
        return no_candidates(
            CLASS_UNASSIGNED,
            format!("{} has no assigned profile in Settings", class_words(class)),
        );
    };
    if !offers
        .iter()
        .any(|o| admits(&harnesses, &o.harness) && admits(&providers, &o.provider))
    {
        let mut allowed = Vec::new();
        match &harnesses {
            Some(h) if h.is_empty() => allowed.push("no harness".to_string()),
            Some(h) => allowed.push(or_list(&harness_list(h))),
            None => {}
        }
        if let Some(p) = &providers {
            let words: Vec<String> = p.iter().map(|x| provider_words(x)).collect();
            allowed.push(format!("using {}", or_list(&words)));
        }
        return no_candidates(
            ALLOWLIST_EXCLUDES_ALL,
            format!(
                "nothing offered at {} is allowed: only {}",
                class_words(class),
                allowed.join(" and ")
            ),
        );
    }
    Plan::Planned(Route {
        class,
        harnesses: harnesses.unwrap_or_default(),
        providers: providers.unwrap_or_default(),
        prefer,
        lane: lane_id.map(String::from),
        provider_intent: if lane.is_some_and(|l| l.hold) {
            ProviderIntent::Pinned
        } else {
            ProviderIntent::Any
        },
        reasons,
        classification: answer.cloned(),
        policy_digest: Some(policy_digest.into()),
        structure_revision: Some(structure_revision),
        source: Source::Router,
    })
}

/// `class`, or Complex, high effort when it is Deep and the task does not
/// need advanced reasoning.
fn deep_only_for_reasoning(class: Class, deep_reasoning: bool) -> Class {
    if class.parent == Parent::Deep && !deep_reasoning {
        COMPLEX_HIGH
    } else {
        class
    }
}

/// [`deep_only_for_reasoning`], recording `deep_requires_reasoning` when it
/// lowers the class. Applied at every step that can change the class.
fn keep_deep_for_reasoning(
    class: Class,
    deep_reasoning: bool,
    reasons: &mut Vec<RouteReason>,
) -> Class {
    let to = deep_only_for_reasoning(class, deep_reasoning);
    if to != class {
        reason(
            reasons,
            "deep_requires_reasoning",
            format!(
                "{} is only for work that needs advanced reasoning, and this task does not: lowered to {}",
                class_words(class),
                class_words(to)
            ),
        );
    }
    to
}

/// Every harness offered at `class`, each once, in offer order.
fn offered_harnesses(options: &Options, class: Class) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for offer in options.classes.get(&class).into_iter().flatten() {
        if !out.contains(&offer.harness) {
            out.push(offer.harness.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::classify::{Flag, Offer, Risk};
    use crate::fixtures;
    use crate::{Parent, ProviderIntent, Source};

    const DIGEST: &str = "sha256:0123456789abcdef";
    const REVISION: u64 = 7;
    const NARROW_TESTED: &[Flag] = &[Flag::Narrow, Flag::TestVerified];
    const NO_FLAGS: &[Flag] = &[];

    fn class(s: &str) -> Class {
        s.parse().unwrap()
    }

    fn answer(kind: &str, level: &str, risk: Risk, flags: &[Flag]) -> Classification {
        Classification {
            task_type: kind.into(),
            intelligence_class: class(level),
            deep_reasoning: flags.contains(&Flag::DeepReasoning),
            narrow: flags.contains(&Flag::Narrow),
            test_verified: flags.contains(&Flag::TestVerified),
            risk,
            risk_reason: "judged".into(),
            reason: "judged".into(),
        }
    }

    fn answered(kind: &str, level: &str, risk: Risk, flags: &[Flag]) -> Classified {
        Classified::Answered(answer(kind, level, risk, flags))
    }

    fn task(kind: &str) -> TaskFacts {
        TaskFacts {
            kind: Some(kind.into()),
            ..TaskFacts::default()
        }
    }

    fn hinted(kind: &str, hint: &str) -> TaskFacts {
        TaskFacts {
            class_hint: Some(class(hint)),
            ..task(kind)
        }
    }

    fn planned(
        policy: &Policy,
        options: &Options,
        task: &TaskFacts,
        classified: &Classified,
    ) -> Route {
        match plan(policy, DIGEST, REVISION, options, task, classified) {
            Plan::Planned(route) => route,
            other => panic!("expected a route for {task:?} {classified:?}: {other:?}"),
        }
    }

    fn reference_route(task: &TaskFacts, classified: &Classified) -> Route {
        let (policy, options) = fixtures::reference();
        planned(&policy, &options, task, classified)
    }

    fn codes(route: &Route) -> Vec<&str> {
        route.reasons.iter().map(|r| r.code.as_str()).collect()
    }

    fn passes(route: &Route, offer: &Offer) -> bool {
        (route.harnesses.is_empty() || route.harnesses.contains(&offer.harness))
            && (route.providers.is_empty() || route.providers.contains(&offer.provider))
    }

    /// Harnesses of the offers at the route's class that its allowlists admit.
    fn admitted(route: &Route, options: &Options) -> Vec<String> {
        options.classes[&route.class]
            .iter()
            .filter(|offer| passes(route, offer))
            .map(|offer| offer.harness.clone())
            .collect()
    }

    fn reference_admitted(route: &Route) -> Vec<String> {
        admitted(route, &fixtures::options())
    }

    #[test]
    fn missing_classification_asks_for_one() {
        let (policy, options) = fixtures::reference();
        let plan = plan(
            &policy,
            DIGEST,
            REVISION,
            &options,
            &task("feature"),
            &Classified::Missing,
        );
        let kinds: Vec<String> = policy.kinds.keys().cloned().collect();
        let classes: Vec<Class> = options.classes.keys().copied().collect();
        assert_eq!(kinds.len(), 14);
        assert_eq!(classes.len(), fixtures::ASSIGNED_CLASSES.len());
        assert_eq!(
            plan,
            Plan::NeedsClassification {
                allowed_kinds: kinds,
                allowed_classes: classes
            }
        );
    }

    #[test]
    fn ordinary_feature_medium_risk_is_standard_high_on_claude_or_codex_with_no_preference() {
        let classified = answered("feature", "standard-high", Risk::Medium, NO_FLAGS);
        let route = reference_route(&TaskFacts::default(), &classified);
        assert_eq!(route.class, class("standard-high"));
        assert_eq!(route.harnesses, ["claude", "codex"]);
        assert!(route.providers.is_empty());
        assert!(route.prefer.is_empty());
        assert_eq!(route.lane, None);
        assert_eq!(route.provider_intent, ProviderIntent::Any);
        assert!(route.reasons.is_empty(), "{:?}", route.reasons);
        assert_eq!(reference_admitted(&route), ["claude", "codex"]);
        let Classified::Answered(expected) = classified else {
            unreachable!()
        };
        assert_eq!(route.classification, Some(expected));
        assert_eq!(route.policy_digest.as_deref(), Some(DIGEST));
        assert_eq!(route.structure_revision, Some(REVISION));
        assert_eq!(route.source, Source::Router);
    }

    #[test]
    fn docs_low_risk_narrow_tested_goes_to_the_local_lane() {
        let route = reference_route(
            &task("docs"),
            &answered("docs", "standard-high", Risk::Low, NARROW_TESTED),
        );
        assert_eq!(route.class, class("standard-high"));
        assert_eq!(route.lane.as_deref(), Some("local-first"));
        assert!(route.harnesses.is_empty() && route.providers.is_empty());
        assert_eq!(route.prefer, ["opencode"]);
        assert_eq!(codes(&route), ["lane_matched"]);
        assert_eq!(
            reference_admitted(&route),
            ["claude", "codex", "gemini", "opencode"]
        );
        // Without a test it is not a cheap lane's work; low risk still restricts nobody.
        let route = reference_route(
            &task("docs"),
            &answered("docs", "standard-high", Risk::Low, &[Flag::Narrow]),
        );
        assert_eq!(route.lane, None);
        assert!(route.prefer.is_empty());
    }

    /// Reference policy, with options that also offer a free hosted model the
    /// policy never names (provider `freehost`, harness `freecode`) at every class.
    #[test]
    fn a_provider_the_policy_never_names_is_excluded_above_low_risk() {
        let (policy, mut options) = fixtures::reference();
        for offers in options.classes.values_mut() {
            offers.push(Offer {
                provider: "freehost".into(),
                harness: "freecode".into(),
            });
        }
        for kind in ["feature", "bugfix", "docs", "chore", "research"] {
            for risk in Risk::ALL {
                for flags in [NO_FLAGS, NARROW_TESTED] {
                    let level = policy.kinds[kind].class.to_string();
                    let route = planned(
                        &policy,
                        &options,
                        &task(kind),
                        &answered(kind, &level, risk, flags),
                    );
                    let free = admitted(&route, &options).contains(&"freecode".to_string());
                    assert_eq!(
                        free,
                        risk == Risk::Low,
                        "{kind} {risk:?} {flags:?}: {route:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn migration_very_high_risk_is_raised_to_complex_low_and_relaxed_to_standard_high_when_narrow_and_tested()
     {
        let raised = reference_route(
            &task("feature"),
            &answered("feature", "standard-high", Risk::VeryHigh, NO_FLAGS),
        );
        assert_eq!(raised.class, class("complex-low"));
        assert_eq!(codes(&raised), ["class_raised_for_risk"]);
        assert_eq!(
            raised.reasons[0].detail,
            "very high risk: raised from Standard, high effort to Complex, low effort"
        );
        assert_eq!(raised.harnesses, ["claude", "codex"]);
        assert_eq!(reference_admitted(&raised), ["claude", "codex"]);

        let only_narrow = reference_route(
            &task("feature"),
            &answered("feature", "standard-high", Risk::VeryHigh, &[Flag::Narrow]),
        );
        assert_eq!(only_narrow.class, class("complex-low"));

        let relaxed = reference_route(
            &task("feature"),
            &answered("feature", "standard-high", Risk::VeryHigh, NARROW_TESTED),
        );
        assert_eq!(relaxed.class, class("standard-high"));
        assert_eq!(codes(&relaxed), ["risk_relaxed"]);
        assert_eq!(relaxed.harnesses, ["claude", "codex"]);
        assert_eq!(relaxed.lane, None, "very high risk takes no cheap lane");
    }

    #[test]
    fn design_goes_to_complex_high_preferring_claude() {
        for risk in Risk::ALL {
            let route = reference_route(
                &task("design"),
                &answered("design", "complex-high", risk, NO_FLAGS),
            );
            assert_eq!(route.class, class("complex-high"), "{risk:?}");
            assert_eq!(route.lane.as_deref(), Some("design"));
            assert_eq!(route.harnesses, ["claude", "codex"]);
            assert_eq!(route.prefer, ["claude"]);
            assert_eq!(route.provider_intent, ProviderIntent::Any);
            assert_eq!(codes(&route), ["lane_matched"]);
            assert_eq!(reference_admitted(&route), ["claude", "codex"]);
        }
    }

    #[test]
    fn art_design_is_pinned_to_codex() {
        for risk in Risk::ALL {
            let route = reference_route(
                &task("art"),
                &answered("art", "standard-high", risk, NARROW_TESTED),
            );
            assert_eq!(route.lane.as_deref(), Some("art"), "{risk:?}");
            assert_eq!(route.harnesses, ["codex"]);
            assert_eq!(route.provider_intent, ProviderIntent::Pinned);
            assert_eq!(reference_admitted(&route), ["codex"]);
        }
    }

    #[test]
    fn deep_hint_without_deep_reasoning_becomes_complex_high_and_with_it_stays_deep() {
        let lowered = reference_route(
            &hinted("analysis", "deep-high"),
            &answered("analysis", "complex-high", Risk::Medium, NO_FLAGS),
        );
        assert_eq!(lowered.class, class("complex-high"));
        assert_eq!(codes(&lowered), ["deep_requires_reasoning", "lane_matched"]);

        let deep = reference_route(
            &hinted("analysis", "deep-high"),
            &answered(
                "analysis",
                "complex-high",
                Risk::Medium,
                &[Flag::DeepReasoning],
            ),
        );
        assert_eq!(deep.class, class("deep-high"));
        assert_eq!(codes(&deep), ["lane_matched"]);
        assert_eq!(reference_admitted(&deep), ["claude", "codex"]);

        // The classifier's own deep answer is held to the same rule.
        let classifier = reference_route(
            &task("spec"),
            &answered("spec", "deep-low", Risk::Low, NO_FLAGS),
        );
        assert_eq!(classifier.class, class("complex-high"));
    }

    #[test]
    fn bugfix_escalates_to_complex_high_after_one_charged_failure_and_ignores_a_lower_hint() {
        let classified = answered("bugfix", "simple-high", Risk::Low, NO_FLAGS);
        let mut facts = hinted("bugfix", "standard-low");
        let first = reference_route(&facts, &classified);
        assert_eq!(first.class, class("standard-high"));
        assert!(first.reasons.is_empty(), "{:?}", first.reasons);
        for failures in [1, 2] {
            facts.failed_attempts = failures;
            let escalated = reference_route(&facts, &classified);
            assert_eq!(escalated.class, class("complex-high"), "{failures}");
            assert_eq!(codes(&escalated), ["escalated_after_failure"]);
        }
        // A higher hint is ignored too: the kind's own level until it escalates.
        let route = reference_route(&hinted("bugfix", "complex-high"), &classified);
        assert_eq!(route.class, class("standard-high"));
    }

    #[test]
    fn hint_above_max_class_is_clamped() {
        let docs = reference_route(
            &hinted("docs", "complex-high"),
            &answered("docs", "standard-high", Risk::Low, NO_FLAGS),
        );
        assert_eq!(docs.class, class("standard-high"));
        assert_eq!(codes(&docs), ["hint_clamped"]);

        let feature = reference_route(
            &hinted("feature", "deep-high"),
            &answered(
                "feature",
                "standard-high",
                Risk::Medium,
                &[Flag::DeepReasoning],
            ),
        );
        assert_eq!(feature.class, class("complex-high"));
        assert_eq!(codes(&feature), ["hint_clamped"]);

        // Without a hint the classifier's level is clamped the same way.
        let chore = reference_route(
            &task("chore"),
            &answered("chore", "complex-low", Risk::Low, NO_FLAGS),
        );
        assert_eq!(chore.class, class("standard-high"));
        assert_eq!(codes(&chore), ["hint_clamped"]);

        // A hint within the kind's range is respected.
        let lower = reference_route(
            &hinted("chore", "standard-medium"),
            &answered("chore", "simple-high", Risk::Low, NO_FLAGS),
        );
        assert_eq!(lower.class, class("standard-medium"));
        assert!(lower.reasons.is_empty());
    }

    #[test]
    fn failed_classification_routes_at_standard_high_on_claude_or_codex_and_never_a_cheap_lane() {
        let (mut policy, options) = fixtures::reference();
        // Even a cheap lane that would admit anything is not taken.
        let open = policy
            .lanes
            .iter_mut()
            .find(|l| l.id == "local-first")
            .unwrap();
        open.requires.clear();
        open.max_risk = Some(Risk::VeryHigh);
        for kind in [
            None,
            Some("feature"),
            Some("docs"),
            Some("chore"),
            Some("unknown"),
        ] {
            let facts = TaskFacts {
                kind: kind.map(String::from),
                ..TaskFacts::default()
            };
            let route = planned(&policy, &options, &facts, &Classified::Failed);
            assert_eq!(route.class, class("standard-high"), "{kind:?}");
            assert_eq!(route.harnesses, ["claude", "codex"], "{kind:?}");
            assert!(route.providers.is_empty());
            assert_eq!(route.lane, None, "{kind:?}");
            assert_eq!(route.classification, None);
            assert_eq!(
                codes(&route)[..2],
                ["classification_failed", "risk_assumed"]
            );
            let raised = kind == Some("chore");
            assert_eq!(
                codes(&route).contains(&"class_raised_for_risk"),
                raised,
                "{kind:?}: {:?}",
                route.reasons
            );
        }
    }

    /// The reference policy's bug fixes escalate to Complex, where nothing is
    /// reserved; a test policy escalating them to Deep shows the reservation bite.
    #[test]
    fn deep_claude_is_removed_for_an_escalated_bugfix_but_kept_for_design() {
        let escalated = TaskFacts {
            failed_attempts: 1,
            ..task("bugfix")
        };
        let reference = reference_route(
            &escalated,
            &answered("bugfix", "standard-high", Risk::Medium, NO_FLAGS),
        );
        assert_eq!(reference.class, class("complex-high"));
        assert!(reference_admitted(&reference).contains(&"claude".to_string()));

        let (mut policy, options) = fixtures::reference();
        let bugfix = policy.kinds.get_mut("bugfix").unwrap();
        bugfix.escalate.as_mut().unwrap().to_class = class("deep-high");
        for risk in [Risk::Low, Risk::Medium] {
            let deep = answered("bugfix", "standard-high", risk, &[Flag::DeepReasoning]);
            let route = planned(&policy, &options, &escalated, &deep);
            assert_eq!(route.class, class("deep-high"), "{risk:?}");
            assert_eq!(route.harnesses, ["codex"], "{risk:?}");
            assert_eq!(
                codes(&route),
                ["escalated_after_failure", "reserved_harness_removed"]
            );
            assert_eq!(admitted(&route, &options), ["codex"]);

            let design = planned(
                &policy,
                &options,
                &hinted("design", "deep-high"),
                &answered("design", "complex-high", risk, &[Flag::DeepReasoning]),
            );
            assert_eq!(design.class, class("deep-high"));
            assert!(!codes(&design).contains(&"reserved_harness_removed"));
            assert_eq!(admitted(&design, &options), ["claude", "codex"]);
        }
    }

    #[test]
    fn class_without_a_profile_is_no_candidates_class_unassigned() {
        let (policy, mut options) = fixtures::reference();
        let simple_medium = plan(
            &policy,
            DIGEST,
            REVISION,
            &options,
            &hinted("chore", "simple-medium"),
            &answered("chore", "simple-high", Risk::Low, NARROW_TESTED),
        );
        let Plan::NoCandidates { code, detail } = simple_medium else {
            panic!("{simple_medium:?}")
        };
        assert_eq!(code, CLASS_UNASSIGNED);
        assert!(detail.contains("Simple, medium effort"), "{detail}");

        options.classes.remove(&class("standard-high"));
        let feature = plan(
            &policy,
            DIGEST,
            REVISION,
            &options,
            &task("feature"),
            &answered("feature", "standard-high", Risk::Medium, NO_FLAGS),
        );
        assert!(
            matches!(&feature, Plan::NoCandidates { code, .. } if code == CLASS_UNASSIGNED),
            "{feature:?}"
        );
    }

    #[test]
    fn allowlist_that_admits_no_offer_is_no_candidates_allowlist_excludes_all() {
        let (policy, mut options) = fixtures::reference();
        options
            .classes
            .get_mut(&class("standard-high"))
            .unwrap()
            .retain(|o| o.harness == "gemini");
        let plan = plan(
            &policy,
            DIGEST,
            REVISION,
            &options,
            &task("feature"),
            &answered("feature", "standard-high", Risk::High, NO_FLAGS),
        );
        let Plan::NoCandidates { code, detail } = plan else {
            panic!("{plan:?}")
        };
        assert_eq!(code, ALLOWLIST_EXCLUDES_ALL);
        assert!(detail.contains("Claude or Codex"), "{detail}");
    }

    #[test]
    fn same_inputs_give_the_same_route() {
        let (policy, options) = fixtures::reference();
        let cases = [
            (
                task("feature"),
                answered("feature", "standard-high", Risk::Medium, NARROW_TESTED),
            ),
            (
                hinted("plan", "deep-high"),
                answered("plan", "deep-high", Risk::High, &[Flag::DeepReasoning]),
            ),
            (task("docs"), Classified::Failed),
        ];
        for (facts, classified) in &cases {
            let first = plan(&policy, DIGEST, REVISION, &options, facts, classified);
            let again = plan(&policy, DIGEST, REVISION, &options, facts, classified);
            assert_eq!(first, again);
            assert!(matches!(first, Plan::Planned(_)), "{first:?}");
        }
    }

    #[test]
    fn very_high_risk_bugfix_is_raised_to_complex_even_though_bugfixes_escalate() {
        let raised = reference_route(
            &task("bugfix"),
            &answered("bugfix", "standard-high", Risk::VeryHigh, NO_FLAGS),
        );
        assert_eq!(raised.class, class("complex-low"));
        assert_eq!(codes(&raised), ["class_raised_for_risk"]);

        let relaxed = reference_route(
            &task("bugfix"),
            &answered("bugfix", "standard-high", Risk::VeryHigh, NARROW_TESTED),
        );
        assert_eq!(relaxed.class, class("standard-high"));
        assert_eq!(codes(&relaxed), ["risk_relaxed"]);

        let escalated = reference_route(
            &TaskFacts {
                failed_attempts: 1,
                ..task("bugfix")
            },
            &answered("bugfix", "standard-high", Risk::VeryHigh, NO_FLAGS),
        );
        assert_eq!(escalated.class, class("complex-high"));
        assert_eq!(codes(&escalated), ["escalated_after_failure"]);
        assert_eq!(escalated.harnesses, ["claude", "codex"]);
    }

    #[test]
    fn cheap_lane_replaces_the_risk_allowlist_so_a_local_model_can_take_narrow_tested_medium_risk_work()
     {
        let route = reference_route(
            &task("feature"),
            &answered("feature", "standard-high", Risk::Medium, NARROW_TESTED),
        );
        assert_eq!(route.class, class("standard-high"));
        assert_eq!(route.lane.as_deref(), Some("local-medium-risk"));
        assert_eq!(route.harnesses, ["claude", "codex", "opencode"]);
        assert_eq!(route.providers, ["anthropic", "openai", "ollama"]);
        assert!(route.prefer.is_empty());
        assert_eq!(codes(&route), ["lane_matched"]);
        assert_eq!(reference_admitted(&route), ["claude", "codex", "opencode"]);
    }

    #[test]
    fn medium_risk_work_that_is_not_narrow_and_tested_never_reaches_a_local_model() {
        let local = |route: &Route| {
            let options = fixtures::options();
            options.classes[&route.class]
                .iter()
                .any(|o| o.provider == "ollama" && passes(route, o))
        };
        for flags in [NO_FLAGS, &[Flag::Narrow][..], &[Flag::TestVerified][..]] {
            for risk in [Risk::Medium, Risk::High] {
                for level in ["standard-low", "standard-medium", "standard-high"] {
                    let route = reference_route(
                        &hinted("feature", level),
                        &answered("feature", level, risk, flags),
                    );
                    assert!(!local(&route), "{flags:?} {risk:?} {level}: {route:?}");
                    assert_eq!(route.harnesses, ["claude", "codex"]);
                }
            }
        }
        // Narrow and tested, but high risk or a repair: still not local.
        let high = reference_route(
            &task("feature"),
            &answered("feature", "standard-high", Risk::High, NARROW_TESTED),
        );
        assert!(!local(&high), "{high:?}");
        let repair = reference_route(
            &TaskFacts {
                origin: Some("repair".into()),
                ..task("feature")
            },
            &answered("feature", "standard-high", Risk::Medium, NARROW_TESTED),
        );
        assert!(!local(&repair), "{repair:?}");
    }

    #[test]
    fn a_kinds_lane_wins_over_an_origins_lane() {
        let repair = |kind: &str| TaskFacts {
            origin: Some("repair".into()),
            ..task(kind)
        };
        let art = reference_route(
            &repair("art"),
            &answered("art", "standard-high", Risk::Low, NO_FLAGS),
        );
        assert_eq!(art.lane.as_deref(), Some("art"));
        assert_eq!(art.harnesses, ["codex"]);
        assert!(art.providers.is_empty());
        assert_eq!(art.provider_intent, ProviderIntent::Pinned);

        let feature = reference_route(
            &repair("feature"),
            &answered("feature", "standard-high", Risk::Low, NARROW_TESTED),
        );
        assert_eq!(feature.lane.as_deref(), Some("repair"));
        assert_eq!(feature.providers, ["anthropic", "openai", "google"]);

        // An origin that only closes the cheap lanes, in a test policy.
        let (mut policy, options) = fixtures::reference();
        policy.origins.insert(
            "hotfix".into(),
            crate::policy::OriginRule {
                narrow: Some(false),
                lane: None,
            },
        );
        let hotfix = planned(
            &policy,
            &options,
            &TaskFacts {
                origin: Some("hotfix".into()),
                ..task("docs")
            },
            &answered("docs", "standard-high", Risk::Low, NARROW_TESTED),
        );
        assert_eq!(hotfix.lane, None);
        assert!(hotfix.prefer.is_empty());
    }

    /// Rule 11 with a test policy: the matched lane's preference wins over the
    /// kind's, and the kind's applies when the lane has none.
    #[test]
    fn lane_preference_wins_over_the_kinds_and_the_kinds_applies_otherwise() {
        let (mut policy, options) = fixtures::reference();
        policy.kinds.get_mut("feature").unwrap().prefer_harnesses = vec!["codex".into()];
        let prefer = |risk: Risk, flags: &[Flag]| {
            planned(
                &policy,
                &options,
                &task("feature"),
                &answered("feature", "standard-high", risk, flags),
            )
            .prefer
        };
        assert_eq!(prefer(Risk::Medium, NO_FLAGS), ["codex"]);
        assert_eq!(prefer(Risk::Low, NARROW_TESTED), ["opencode"]);
        assert_eq!(prefer(Risk::Medium, NARROW_TESTED), ["codex"]);
    }

    #[test]
    fn the_reference_policy_routes_the_nine_reference_cases_as_the_prose_says() {
        struct Row {
            case: &'static str,
            task: TaskFacts,
            classified: Classified,
            class: &'static str,
            admitted: &'static [&'static str],
            prefer: &'static [&'static str],
            pinned: bool,
        }
        let deep = &[Flag::DeepReasoning][..];
        let rows = [
            Row {
                case: "a Deep plan",
                task: task("plan"),
                classified: answered("plan", "deep-high", Risk::Medium, deep),
                class: "deep-high",
                admitted: &["claude", "codex"],
                prefer: &["claude"],
                pinned: false,
            },
            Row {
                case: "a Deep spec",
                task: task("spec"),
                classified: answered("spec", "deep-low", Risk::Medium, deep),
                class: "deep-low",
                admitted: &["claude", "codex"],
                prefer: &[],
                pinned: false,
            },
            Row {
                case: "a Complex design",
                task: task("design"),
                classified: answered("design", "complex-high", Risk::Medium, NO_FLAGS),
                class: "complex-high",
                admitted: &["claude", "codex"],
                prefer: &["claude"],
                pinned: false,
            },
            Row {
                case: "an art task",
                task: task("art"),
                classified: answered("art", "standard-high", Risk::Low, NARROW_TESTED),
                class: "standard-high",
                admitted: &["codex"],
                prefer: &[],
                pinned: true,
            },
            Row {
                case: "a narrow, tested, low risk docs task",
                task: task("docs"),
                classified: answered("docs", "standard-high", Risk::Low, NARROW_TESTED),
                class: "standard-high",
                admitted: &["claude", "codex", "gemini", "opencode"],
                prefer: &["opencode"],
                pinned: false,
            },
            Row {
                case: "a narrow, tested, medium risk feature",
                task: task("feature"),
                classified: answered("feature", "standard-high", Risk::Medium, NARROW_TESTED),
                class: "standard-high",
                admitted: &["claude", "codex", "opencode"],
                prefer: &[],
                pinned: false,
            },
            Row {
                case: "a medium risk feature that is not narrow",
                task: task("feature"),
                classified: answered(
                    "feature",
                    "standard-high",
                    Risk::Medium,
                    &[Flag::TestVerified],
                ),
                class: "standard-high",
                admitted: &["claude", "codex"],
                prefer: &[],
                pinned: false,
            },
            Row {
                case: "a high risk feature",
                task: task("feature"),
                classified: answered("feature", "standard-high", Risk::High, NARROW_TESTED),
                class: "standard-high",
                admitted: &["claude", "codex"],
                prefer: &[],
                pinned: false,
            },
            Row {
                case: "a repair of a low risk change",
                task: TaskFacts {
                    origin: Some("repair".into()),
                    ..task("feature")
                },
                classified: answered("feature", "standard-high", Risk::Low, NARROW_TESTED),
                class: "standard-high",
                admitted: &["claude", "codex", "gemini"],
                prefer: &[],
                pinned: false,
            },
        ];
        let options = fixtures::options();
        for row in rows {
            let route = reference_route(&row.task, &row.classified);
            let case = row.case;
            assert_eq!(route.class, class(row.class), "{case}: {route:?}");
            assert_eq!(
                admitted(&route, &options),
                row.admitted,
                "{case}: {route:?}"
            );
            assert_eq!(route.prefer, row.prefer, "{case}");
            let intent = if row.pinned {
                ProviderIntent::Pinned
            } else {
                ProviderIntent::Any
            };
            assert_eq!(route.provider_intent, intent, "{case}");
            let local = options.classes[&route.class]
                .iter()
                .any(|o| o.provider == "ollama" && passes(&route, o));
            assert_eq!(local, row.admitted.contains(&"opencode"), "{case}");
        }
    }

    /// The reason codes listed on `RouteReason` in lib.rs.
    const REASON_CODES: [&str; 10] = [
        "hint_clamped",
        "class_raised_for_risk",
        "risk_relaxed",
        "risk_assumed",
        "escalated_after_failure",
        "deep_requires_reasoning",
        "lane_matched",
        "reserved_harness_removed",
        "classification_failed",
        "owner_set",
    ];

    fn lane_mut<'a>(policy: &'a mut Policy, id: &str) -> &'a mut Lane {
        policy.lanes.iter_mut().find(|l| l.id == id).unwrap()
    }

    fn cheap_lane(id: &str, map: &[(&str, &str)], requires: &[Flag], max_risk: Risk) -> Lane {
        Lane {
            id: id.into(),
            class: None,
            classes: map
                .iter()
                .map(|(from, to)| (class(from), class(to)))
                .collect(),
            harnesses: vec![],
            providers: vec![],
            prefer: vec![],
            hold: false,
            requires: requires.to_vec(),
            max_risk: Some(max_risk),
            cheap: true,
        }
    }

    fn plain_lane(id: &str, level: &str) -> Lane {
        Lane {
            class: Some(class(level)),
            classes: BTreeMap::new(),
            max_risk: None,
            cheap: false,
            ..cheap_lane(id, &[], &[], Risk::Low)
        }
    }

    /// A test policy built to break the Deep and risk-floor rules: the design
    /// lane sets Deep, docs go through a lane that sets Simple, two cheap
    /// lanes ahead of the reference ones map Standard, high effort to Deep and
    /// to Standard, low effort for work up to medium risk, and high risk has a
    /// Deep floor (which `validate` refuses).
    fn hostile() -> (Policy, Options) {
        let (mut policy, options) = fixtures::reference();
        policy.risk.get_mut(&Risk::High).unwrap().min_class = class("deep-low");
        lane_mut(&mut policy, "design").class = Some(class("deep-high"));
        policy.lanes.push(plain_lane("quick", "simple-high"));
        policy.kinds.get_mut("docs").unwrap().lane = Some("quick".into());
        let deep = cheap_lane(
            "cheap-deep",
            &[("standard-high", "deep-low")],
            &[Flag::Narrow],
            Risk::Medium,
        );
        let low = cheap_lane(
            "cheap-low",
            &[("standard-high", "standard-low")],
            &[],
            Risk::Medium,
        );
        policy.lanes.splice(0..0, [low, deep]);
        (policy, options)
    }

    /// The floor rule 8 sets for the classification's risk and flags, capped
    /// at Complex, high effort when it is Deep and the task does not need
    /// advanced reasoning (the Deep rule wins).
    fn risk_floor(policy: &Policy, classified: &Classified) -> Option<Class> {
        let answer = match classified {
            Classified::Answered(c) => Some(c),
            _ => None,
        };
        let rule = policy.risk.get(&answer.map_or(Risk::Medium, |c| c.risk))?;
        let set = |f: &Flag| {
            answer.is_some_and(|c| match f {
                Flag::Narrow => c.narrow,
                Flag::TestVerified => c.test_verified,
                Flag::DeepReasoning => c.deep_reasoning,
            })
        };
        let floor = match &rule.relax {
            Some(relax) if relax.requires.iter().all(set) => relax.class,
            _ => rule.min_class,
        };
        Some(deep_only_for_reasoning(floor, set(&Flag::DeepReasoning)))
    }

    /// Every kind × origin × failed attempts × hint (none, one level below
    /// and one above the kind's highest) × risk × flags × level answered,
    /// plus every failed classification: each is a consistent, repeatable
    /// route. Returns how many were planned.
    fn check_every_plan(policy: &Policy, options: &Options) -> usize {
        let flags = [Flag::Narrow, Flag::TestVerified, Flag::DeepReasoning];
        let levels: Vec<Class> = options.classes.keys().copied().collect();
        let check = |facts: &TaskFacts, classified: &Classified| {
            let route = planned(policy, options, facts, classified);
            let at = format!("{facts:?} {classified:?}: {route:?}");
            assert!(options.classes.contains_key(&route.class), "{at}");
            assert!(
                options.classes[&route.class]
                    .iter()
                    .any(|o| passes(&route, o)),
                "{at}"
            );
            assert_eq!(route.policy_digest.as_deref(), Some(DIGEST), "{at}");
            assert_eq!(route.structure_revision, Some(REVISION), "{at}");
            assert_eq!(route.source, Source::Router, "{at}");
            match classified {
                Classified::Answered(c) => assert_eq!(route.classification.as_ref(), Some(c)),
                _ => assert_eq!(route.classification, None, "{at}"),
            }
            if let Some(lane) = &route.lane {
                assert!(policy.lanes.iter().any(|l| &l.id == lane), "{at}");
            }
            if route.class.parent == Parent::Deep {
                let Classified::Answered(c) = classified else {
                    panic!("Deep without a classification: {at}")
                };
                assert!(c.deep_reasoning, "Deep without deep_reasoning: {at}");
            }
            if let Some(floor) = risk_floor(policy, classified) {
                assert!(route.class >= floor, "below the risk floor {floor}: {at}");
            }
            for r in &route.reasons {
                assert!(REASON_CODES.contains(&r.code.as_str()), "{}: {at}", r.code);
            }
            let again = planned(policy, options, facts, classified);
            assert_eq!(again, route);
        };
        let mut count = 0;
        for (id, kind) in &policy.kinds {
            let below = levels.iter().rev().find(|c| **c < kind.max_class).copied();
            let above = levels.iter().find(|c| **c > kind.max_class).copied();
            let hints = [None]
                .into_iter()
                .chain([below, above].map(|h| h.map(Some)).into_iter().flatten());
            for class_hint in hints {
                for origin in [None, Some("repair".to_string())] {
                    for failed_attempts in [0, 1] {
                        let facts = TaskFacts {
                            kind: Some(id.clone()),
                            class_hint,
                            origin: origin.clone(),
                            failed_attempts,
                        };
                        check(&facts, &Classified::Failed);
                        count += 1;
                        for risk in Risk::ALL {
                            for set in 0..8u8 {
                                let on: Vec<Flag> = (0..3)
                                    .filter(|bit| set & (1 << bit) != 0)
                                    .map(|bit| flags[bit])
                                    .collect();
                                for level in [kind.class, kind.max_class] {
                                    let classified = answered(id, &level.to_string(), risk, &on);
                                    check(&facts, &classified);
                                    count += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        count
    }

    #[test]
    fn every_reference_plan_is_a_consistent_route_and_planning_twice_gives_the_same() {
        let (policy, options) = fixtures::reference();
        let count = check_every_plan(&policy, &options);
        assert!(
            count >= policy.kinds.len() * 2 * 2 * 2 * 4 * 8 * 2,
            "{count}"
        );
    }

    #[test]
    fn a_policy_built_to_break_the_deep_and_risk_floor_rules_still_plans_consistent_routes() {
        let (policy, options) = hostile();
        let count = check_every_plan(&policy, &options);
        assert!(
            count >= policy.kinds.len() * 2 * 2 * 2 * 4 * 8 * 2,
            "{count}"
        );
    }

    #[test]
    fn a_lane_that_sets_deep_still_needs_deep_reasoning() {
        let (policy, options) = hostile();
        let without = planned(
            &policy,
            &options,
            &task("design"),
            &answered("design", "complex-high", Risk::Medium, NO_FLAGS),
        );
        assert_eq!(without.class, class("complex-high"));
        assert_eq!(codes(&without), ["lane_matched", "deep_requires_reasoning"]);
        let with = planned(
            &policy,
            &options,
            &task("design"),
            &answered(
                "design",
                "complex-high",
                Risk::Medium,
                &[Flag::DeepReasoning],
            ),
        );
        assert_eq!(with.class, class("deep-high"));
        assert_eq!(codes(&with), ["lane_matched"]);
    }

    #[test]
    fn a_cheap_lane_that_maps_to_deep_still_needs_deep_reasoning() {
        let (policy, options) = hostile();
        let without = planned(
            &policy,
            &options,
            &task("feature"),
            &answered("feature", "standard-high", Risk::Medium, &[Flag::Narrow]),
        );
        assert_eq!(without.lane.as_deref(), Some("cheap-deep"));
        assert_eq!(without.class, class("complex-high"));
        assert_eq!(codes(&without), ["lane_matched", "deep_requires_reasoning"]);
        let with = planned(
            &policy,
            &options,
            &task("feature"),
            &answered(
                "feature",
                "standard-high",
                Risk::Medium,
                &[Flag::Narrow, Flag::DeepReasoning],
            ),
        );
        assert_eq!(with.lane.as_deref(), Some("cheap-deep"));
        assert_eq!(with.class, class("deep-low"));
    }

    #[test]
    fn a_cheap_lane_that_maps_below_the_risk_floor_is_skipped_for_the_next() {
        let (policy, options) = hostile();
        // Medium risk: the floor is Standard, high effort, so the lane mapping
        // to Standard, low effort is skipped.
        let medium = planned(
            &policy,
            &options,
            &task("feature"),
            &answered("feature", "standard-high", Risk::Medium, NO_FLAGS),
        );
        assert_eq!(medium.class, class("standard-high"));
        assert_eq!(medium.lane, None);
        assert_eq!(medium.harnesses, ["claude", "codex"]);
        // Low risk has no floor: the first cheap lane takes it.
        let low = planned(
            &policy,
            &options,
            &task("feature"),
            &answered("feature", "standard-high", Risk::Low, NO_FLAGS),
        );
        assert_eq!(low.lane.as_deref(), Some("cheap-low"));
        assert_eq!(low.class, class("standard-low"));
    }

    #[test]
    fn a_deep_risk_floor_raises_only_work_that_needs_advanced_reasoning() {
        let (policy, options) = hostile();
        let without = planned(
            &policy,
            &options,
            &task("feature"),
            &answered("feature", "standard-high", Risk::High, NO_FLAGS),
        );
        assert_eq!(without.class, class("complex-high"));
        assert_eq!(
            codes(&without),
            ["class_raised_for_risk", "deep_requires_reasoning"]
        );
        let with = planned(
            &policy,
            &options,
            &task("feature"),
            &answered(
                "feature",
                "standard-high",
                Risk::High,
                &[Flag::DeepReasoning],
            ),
        );
        assert_eq!(with.class, class("deep-low"));
    }

    #[test]
    fn a_lane_class_below_the_risk_floor_is_raised_to_it() {
        let (policy, options) = hostile();
        let medium = planned(
            &policy,
            &options,
            &task("docs"),
            &answered("docs", "standard-high", Risk::Medium, NO_FLAGS),
        );
        assert_eq!(medium.lane.as_deref(), Some("quick"));
        assert_eq!(medium.class, class("standard-high"));
        assert_eq!(codes(&medium), ["lane_matched", "class_raised_for_risk"]);
        let low = planned(
            &policy,
            &options,
            &task("docs"),
            &answered("docs", "standard-high", Risk::Low, NO_FLAGS),
        );
        assert_eq!(low.class, class("simple-high"));
    }

    #[test]
    fn a_kinds_lane_class_replaces_the_requested_level() {
        let (mut policy, options) = fixtures::reference();
        lane_mut(&mut policy, "art").class = Some(class("complex-low"));
        let route = planned(
            &policy,
            &options,
            &hinted("art", "standard-medium"),
            &answered("art", "standard-high", Risk::Low, NO_FLAGS),
        );
        assert_eq!(route.class, class("complex-low"));
        assert_eq!(codes(&route), ["lane_matched"]);
        assert!(
            route.reasons[0]
                .detail
                .contains("sets Complex, low effort in place of Standard, medium effort"),
            "{:?}",
            route.reasons
        );
    }

    #[test]
    fn an_escalated_bugfix_keeps_its_class_through_a_lane_with_a_class() {
        let (mut policy, options) = fixtures::reference();
        policy.lanes.push(plain_lane("fixes", "standard-medium"));
        policy.kinds.get_mut("bugfix").unwrap().lane = Some("fixes".into());
        let classified = answered("bugfix", "standard-high", Risk::Low, NO_FLAGS);
        let first = planned(&policy, &options, &task("bugfix"), &classified);
        assert_eq!(first.class, class("standard-medium"));
        let escalated = TaskFacts {
            failed_attempts: 1,
            ..task("bugfix")
        };
        let route = planned(&policy, &options, &escalated, &classified);
        assert_eq!(route.class, class("complex-high"));
        assert_eq!(codes(&route), ["escalated_after_failure", "lane_matched"]);
    }

    #[test]
    fn a_cheap_lane_that_maps_to_another_level_moves_the_class_and_says_so() {
        let (mut policy, options) = fixtures::reference();
        let local = lane_mut(&mut policy, "local-first");
        local
            .classes
            .insert(class("standard-high"), class("standard-medium"));
        let route = planned(
            &policy,
            &options,
            &task("docs"),
            &answered("docs", "standard-high", Risk::Low, NARROW_TESTED),
        );
        assert_eq!(route.lane.as_deref(), Some("local-first"));
        assert_eq!(route.class, class("standard-medium"));
        assert_eq!(codes(&route), ["lane_matched"]);
        assert!(
            route.reasons[0]
                .detail
                .ends_with("moves Standard, high effort to Standard, medium effort"),
            "{:?}",
            route.reasons
        );
    }

    #[test]
    fn a_cheap_lane_with_no_admissible_offer_falls_through_to_the_next() {
        let (mut policy, options) = fixtures::reference();
        let mut gemini_only = cheap_lane(
            "gemini-only",
            &[("standard-medium", "standard-medium")],
            NARROW_TESTED,
            Risk::Low,
        );
        // Settings offers no Gemini at Standard, medium effort.
        gemini_only.harnesses = vec!["gemini".into()];
        policy.lanes.insert(0, gemini_only);
        let route = planned(
            &policy,
            &options,
            &hinted("chore", "standard-medium"),
            &answered("chore", "simple-high", Risk::Low, NARROW_TESTED),
        );
        assert_eq!(route.lane.as_deref(), Some("local-first"));
        assert_eq!(route.class, class("standard-medium"));
        assert_eq!(route.prefer, ["opencode"]);
    }

    #[test]
    fn a_cheap_lane_without_a_risk_ceiling_is_never_used() {
        let (mut policy, options) = fixtures::reference();
        policy.lanes.retain(|l| l.id != "local-medium-risk");
        let local = lane_mut(&mut policy, "local-first");
        local.max_risk = None;
        for risk in Risk::ALL {
            let route = planned(
                &policy,
                &options,
                &task("feature"),
                &answered("feature", "standard-high", risk, NARROW_TESTED),
            );
            assert_eq!(route.lane, None, "{risk:?}");
            if risk >= Risk::High {
                assert_eq!(route.harnesses, ["claude", "codex"], "{risk:?}");
            }
        }
    }
}

//! Host catalog 2.3.0 (Host10), pure parts (Epic 0, spec 0016 section 6):
//! the facts of a task's latest result for `work.read`, and the
//! `aq.work.result_ready` event with its level-triggered emission rule. No
//! I/O. Compiled in only with the cargo feature `delivery_playbook`.
//!
//! Code provides facts and events; a playbook decides. Nothing here says what
//! a result is worth.
use crate::host9::{self, WorkFacts, truncate};
use serde_json::{Value, json};
use uuid::Uuid;

/// The playbook binding that delivers results (spec 0016 section 8). The
/// event `aq.work.result_ready` is sent only while an artifact is active on it.
pub const DELIVERY_BINDING: &str = "aq.delivery";

const OUTCOME_BYTES: usize = 64;
const BRANCH_BYTES: usize = 256;
const OBJECT_BYTES: usize = 128;
const URL_BYTES: usize = 2048;
const CHECK_NAME_BYTES: usize = 256;
const CHECK_NAMES: usize = 64;
const RETURNS_MAX: u32 = 1000;
const PULL_REQUEST_MAX: u64 = 1_000_000_000;

/// The branch of a task's result in the central git (`aq/<task-id>`).
pub fn branch_of(task: Uuid) -> String {
    format!("aq/{task}")
}

/// The pull request of a task as the host last knew it (`work.read`
/// `pull_request`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestFacts {
    pub number: u64,
    pub url: String,
    /// `open`, `closed` or `merged`.
    pub state: String,
    /// `mergeable`, `conflicting` or `unknown`.
    pub mergeable: String,
    /// `none`, `pending`, `passed` or `failed`.
    pub checks_state: String,
    /// The names of the failed checks.
    pub checks_failed: Vec<String>,
}

impl PullRequestFacts {
    /// The facts as the catalog declares them: the strings cut to their
    /// bounds. `None` when a value is outside what the catalog can express (a
    /// number outside 1..=1e9, or a state it does not list): the host then
    /// knows no pull request rather than say something untrue.
    pub fn normalized(&self) -> Option<Self> {
        let one_of = |value: &str, values: &[&str]| values.contains(&value);
        if !(1..=PULL_REQUEST_MAX).contains(&self.number)
            || !one_of(&self.state, &["open", "closed", "merged"])
            || !one_of(&self.mergeable, &["mergeable", "conflicting", "unknown"])
            || !one_of(&self.checks_state, &["none", "pending", "passed", "failed"])
        {
            return None;
        }
        Some(Self {
            number: self.number,
            url: truncate(&self.url, URL_BYTES).into(),
            state: self.state.clone(),
            mergeable: self.mergeable.clone(),
            checks_state: self.checks_state.clone(),
            checks_failed: (self.checks_failed.iter())
                .take(CHECK_NAMES)
                .map(|n| truncate(n, CHECK_NAME_BYTES).to_owned())
                .collect(),
        })
    }
    fn output(&self) -> Value {
        json!({
            "number": self.number, "url": self.url, "state": self.state,
            "mergeable": self.mergeable,
            "checks": {"state": self.checks_state, "failed": self.checks_failed},
        })
    }
}

/// The facts of a task's latest result (`work.read`). Every field is always
/// present in the answer; `Default` is "the task has no result yet".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResultFacts {
    /// The outcome the agent reported, or `""`.
    pub outcome: String,
    /// The result differs from `base`.
    pub changed: bool,
    /// The result's branch in the central git, or `""`.
    pub branch: String,
    /// The commit the branch points at, or `""`.
    pub head: String,
    /// The commit of main the result was made on, or `""`.
    pub base: String,
    /// The commit main points at now, or `""`.
    pub main_head: String,
    /// How many times delivery sent the task back to its agent.
    pub returns: u32,
    pub pull_request: Option<PullRequestFacts>,
}

impl ResultFacts {
    pub fn output(&self) -> Value {
        json!({
            "outcome": truncate(&self.outcome, OUTCOME_BYTES),
            "changed": self.changed,
            "branch": truncate(&self.branch, BRANCH_BYTES),
            "head": truncate(&self.head, OBJECT_BYTES),
            "base": truncate(&self.base, OBJECT_BYTES),
            "main_head": truncate(&self.main_head, OBJECT_BYTES),
            "returns": self.returns.min(RETURNS_MAX),
            "pull_request": self.pull_request.as_ref().and_then(PullRequestFacts::normalized)
                .map_or(Value::Null, |p| p.output()),
        })
    }
}

/// The `observed` output of `work.read` under catalog 2.3.0: the task's facts
/// of 2.2.0 and those of its latest result.
pub fn read_output(task: Uuid, version: i64, facts: &WorkFacts, result: &ResultFacts) -> Value {
    let mut output = host9::read_output(task, version, facts);
    if let (Some(all), Some(more)) = (output["facts"].as_object_mut(), result.output().as_object())
    {
        all.extend(more.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    output
}

/// What a playbook can read that moves without the result changing: the
/// commit main points at and the pull request's state. A different value
/// than the one the last `aq.work.result_ready` was sent under is a reason to
/// send it again.
pub fn facts_digest(main_head: &str, pull_request: Option<&PullRequestFacts>) -> String {
    let pull_request = pull_request.and_then(PullRequestFacts::normalized);
    let value = json!({
        "main_head": main_head,
        "pull_request": pull_request.map_or(Value::Null, |p| p.output()),
    });
    aq_playbook_host::model::digest(&value).unwrap_or_default()
}

// ------------------------------------------------------ aq.work.result_ready

/// One task's emission row, as the emitter reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResultEmission {
    /// The result the event was last sent for.
    pub emitted_result: Option<Uuid>,
    pub emitted_at_ms: i64,
    /// [`facts_digest`] when it was last sent.
    pub emitted_facts: Option<String>,
    /// The previous event is queued or its delivery run has not finished.
    pub run_unfinished: bool,
}

/// Whether to send `aq.work.result_ready` for `result` now. A result never
/// sent for is sent at once. Otherwise never while a run started for it is
/// unfinished; then at once when `facts` (see [`facts_digest`]) are not those
/// of the last send, else again after `repeat_ms` (Settings, `Delivery limits`
/// `decision_repeat_ms`).
pub fn result_ready_due(
    e: &ResultEmission,
    result: Uuid,
    facts: &str,
    now_ms: i64,
    repeat_ms: i64,
) -> bool {
    if e.emitted_result != Some(result) {
        return true;
    }
    if e.run_unfinished {
        return false;
    }
    e.emitted_facts.as_deref() != Some(facts) || now_ms - e.emitted_at_ms >= repeat_ms
}

/// The ids an `aq.work.result_ready` event carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ResultReady {
    pub task_id: Uuid,
    pub project_id: Uuid,
    pub attempt_id: Uuid,
    pub result_id: Uuid,
    /// The task's version.
    pub version: i64,
}

impl ResultReady {
    /// Whether the command names this event's task and report.
    pub fn names_result(&self, input: &Value) -> bool {
        input["task"] == self.task_id.to_string()
            && input["result_id"] == self.result_id.to_string()
    }
}

/// The trusted `aq.work.result_ready` event.
pub fn result_ready_event(
    event_id: Uuid,
    ready: &ResultReady,
    now_ms: i64,
    contract: &aq_playbook_host::model::EventContract,
) -> Result<aq_playbook_host::model::EventEnvelope, aq_playbook_host::model::ModelError> {
    let version = ready.version.max(0) as u64;
    Ok(aq_playbook_host::model::EventEnvelope {
        producer: "aq.host.bridge@1".into(),
        event_id: event_id.to_string(),
        contract: aq_playbook_host::model::ContractRef {
            name: contract.name.clone(),
            fingerprint: contract.fingerprint()?,
        },
        scope: "project".into(),
        correlation: Default::default(),
        subject: Some(ready.task_id.to_string()),
        revision: Some(version),
        causation: None,
        occurred_at_ms: Some(now_ms),
        observed_at_ms: Some(now_ms),
        payload: json!({
            "task_id": ready.task_id.to_string(),
            "project_id": ready.project_id.to_string(),
            "attempt_id": ready.attempt_id.to_string(),
            "result_id": ready.result_id.to_string(),
            "version": version,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review_domain::CatalogProfile;

    fn ids() -> (Uuid, Uuid) {
        (Uuid::from_u128(1), Uuid::from_u128(2))
    }
    fn pull_request() -> PullRequestFacts {
        PullRequestFacts {
            number: 7,
            url: "https://example.test/pr/7".into(),
            state: "open".into(),
            mergeable: "unknown".into(),
            checks_state: "failed".into(),
            checks_failed: vec!["unit".into()],
        }
    }

    #[test]
    fn an_unsent_result_is_sent_and_a_sent_one_is_repeated_only_after_the_setting() {
        let (result, other) = ids();
        let sent = ResultEmission {
            emitted_result: Some(result),
            emitted_at_ms: 1_000,
            emitted_facts: Some("a".into()),
            run_unfinished: false,
        };
        assert!(result_ready_due(
            &ResultEmission::default(),
            result,
            "a",
            0,
            120_000
        ));
        // Not repeated before the interval, repeated at it.
        assert!(!result_ready_due(&sent, result, "a", 120_999, 120_000));
        assert!(result_ready_due(&sent, result, "a", 121_000, 120_000));
        // A new result is sent at once, even while a run for the old one is unfinished.
        let busy = ResultEmission {
            run_unfinished: true,
            ..sent.clone()
        };
        assert!(result_ready_due(&busy, other, "a", 1_001, 120_000));
    }

    #[test]
    fn nothing_is_sent_while_a_run_for_the_result_is_unfinished() {
        let (result, _) = ids();
        let busy = ResultEmission {
            emitted_result: Some(result),
            emitted_at_ms: 1_000,
            emitted_facts: Some("a".into()),
            run_unfinished: true,
        };
        // Neither the interval nor a change of the facts overrides it...
        assert!(!result_ready_due(&busy, result, "a", 10_000_000, 120_000));
        assert!(!result_ready_due(&busy, result, "b", 1_001, 120_000));
        // ...and the run's end releases a change at once.
        let ended = ResultEmission {
            run_unfinished: false,
            ..busy
        };
        assert!(result_ready_due(&ended, result, "b", 1_001, 120_000));
    }

    #[test]
    fn the_facts_digest_moves_with_main_and_with_the_pull_request_only() {
        let open = pull_request();
        let base = facts_digest("aaa", Some(&open));
        assert_eq!(base, facts_digest("aaa", Some(&open.clone())));
        assert_ne!(base, facts_digest("bbb", Some(&open)));
        assert_ne!(base, facts_digest("aaa", None));
        let passed = PullRequestFacts {
            checks_state: "passed".into(),
            checks_failed: vec![],
            ..open.clone()
        };
        assert_ne!(base, facts_digest("aaa", Some(&passed)));
        // A pull request the catalog cannot express is no pull request.
        let odd = PullRequestFacts { number: 0, ..open };
        assert_eq!(facts_digest("aaa", Some(&odd)), facts_digest("aaa", None));
    }

    #[test]
    fn the_event_is_the_catalogs_and_carries_the_ids() {
        let registry = CatalogProfile::Host10.registry();
        let contract = &registry.events["aq.work.result_ready"];
        let ready = ResultReady {
            task_id: Uuid::from_u128(1),
            project_id: Uuid::from_u128(2),
            attempt_id: Uuid::from_u128(3),
            result_id: Uuid::from_u128(4),
            version: 5,
        };
        let event = result_ready_event(Uuid::from_u128(9), &ready, 10, contract).unwrap();
        aq_playbook_host::model::validate_event(&event, &registry).unwrap();
        assert_eq!(
            event.subject.as_deref(),
            Some(ready.task_id.to_string().as_str())
        );
        assert_eq!(event.revision, Some(5));
        assert!(event.correlation.is_empty());
        assert_eq!(event.payload["result_id"], ready.result_id.to_string());
        assert_eq!(event.payload["version"], 5);
    }

    #[test]
    fn work_read_answers_the_catalogs_observed_output_with_and_without_a_result() {
        let registry = CatalogProfile::Host10.registry();
        let observed = &registry.commands["work.read"].outcomes["observed"];
        let task = Uuid::from_u128(1);
        let node = json!({"title": "t", "body": "b", "metadata": {}});
        let work = WorkFacts::of(&node, false, 0, 0);
        // No result yet: every fact present and empty.
        let none = read_output(task, 3, &work, &ResultFacts::default());
        observed.validate(&none).unwrap();
        assert_eq!(none["facts"]["branch"], "");
        assert_eq!(none["facts"]["changed"], false);
        assert_eq!(none["facts"]["returns"], 0);
        assert!(none["facts"]["pull_request"].is_null());
        // A result with a pull request.
        let result = ResultFacts {
            outcome: "success".into(),
            changed: true,
            branch: branch_of(task),
            head: "h".repeat(40),
            base: "b".repeat(40),
            main_head: "m".repeat(40),
            returns: 2,
            pull_request: Some(pull_request()),
        };
        let some = read_output(task, 3, &work, &result);
        observed.validate(&some).unwrap();
        assert_eq!(some["facts"]["returns"], 2);
        assert_eq!(some["facts"]["pull_request"]["number"], 7);
        assert_eq!(some["facts"]["pull_request"]["checks"]["failed"][0], "unit");
        assert_eq!(some["facts"]["branch"], format!("aq/{task}"));
        // The 2.2.0 facts are unchanged.
        assert_eq!(some["facts"]["title"], "t");
    }
}

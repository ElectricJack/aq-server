//! Synthetic domain services for contract tests. This is deliberately not an AQ
//! adapter. Domain records and notification intents belong to this host, separate
//! from runner state; serializing `FakeHost` simulates a durable host restart.
//! Named `context.read` snapshots are immutable retained observations. The one
//! supported fresh-read mode uses `snapshot=""`, `view="project"`, `page=0`, and
//! subject id `main`; its input revision is a request hint, while the returned
//! subject revision comes from the current host record. Every fresh read records
//! its own bounded immutable snapshot and requires project visibility.
use crate::catalog;
use aq_playbooks_model::{CommandContract, ContractRef, EventEnvelope, digest};
use aq_playbooks_runner::{Authority, CancellationResult, DispatchResult, Host, Intent};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Subject {
    pub id: String,
    pub revision: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hold {
    pub subject: Subject,
    pub actor: String,
    pub scope: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub subject: Subject,
    pub suite: String,
    pub passed: bool,
    pub issuer: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub subject: Subject,
    pub base: Subject,
    pub sources: Vec<Subject>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Work {
    pub reference: String,
    pub subject: Subject,
    pub kind: String,
    pub visibility: String,
    pub request_digest: String,
    pub authored_result: Option<String>,
    pub lifetime: String,
    pub operation_id: String,
    pub completion: Option<Value>,
    pub cancelled: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub reference: String,
    pub partition: String,
    pub scope: String,
    pub visibility: String,
    pub view: String,
    pub subject: Subject,
    pub pages: Vec<Value>,
    pub complete: bool,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
    pub episode_revisions: BTreeMap<String, u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub revision: u64,
    pub active: bool,
    pub evidence: String,
    pub last_notification_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evaluation {
    pub reference: String,
    pub observation: String,
    pub partition: String,
    pub scope: String,
    pub findings_digest: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub identity: String,
    pub body: String,
    pub acknowledged: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationReceipt {
    pub request_digest: String,
    pub actor: String,
    pub scope: String,
    pub result: DispatchResult,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarOccurrence {
    pub identity: String,
    pub schedule: String,
    pub scheduled_at_ms: u64,
    pub timezone: String,
    pub misfire: String,
    pub configuration_revision: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarConfiguration {
    pub revision: u64,
    pub timezone: String,
    pub misfire: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FakeHost {
    pub now_ms: u64,
    pub subjects: BTreeMap<String, u64>,
    pub grants: BTreeMap<String, BTreeSet<String>>,
    pub visibility: BTreeMap<String, BTreeSet<String>>,
    pub holds: BTreeMap<String, Hold>,
    pub evidence: BTreeMap<String, Evidence>,
    pub candidates: BTreeMap<String, Candidate>,
    pub work: BTreeMap<String, Work>,
    pub observations: BTreeMap<String, Observation>,
    pub evaluations: BTreeMap<String, Evaluation>,
    pub episodes: BTreeMap<String, Episode>,
    pub outbox: BTreeMap<String, Notification>,
    pub recipient_receipts: BTreeMap<String, String>,
    pub operations: BTreeMap<String, OperationReceipt>,
    pub messages: BTreeMap<String, Value>,
    pub calendars: BTreeMap<String, CalendarConfiguration>,
    pub occurrences: BTreeMap<String, CalendarOccurrence>,
    /// Explicit fake outcomes, keyed by subject id/revision/suite or operation id.
    pub configured_outcomes: BTreeMap<String, String>,
    pub configured_choices: BTreeMap<String, String>,
    /// Emulates a lost transport response after the domain transaction committed.
    pub lose_next_response: bool,
}

fn field<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}"))
}
fn parse_subject(value: &Value) -> Result<Subject, String> {
    serde_json::from_value(value.clone()).map_err(|_| "invalid subject".into())
}
fn subject_field(input: &Value, key: &str) -> Result<Subject, String> {
    parse_subject(&input[key])
}
fn result(reference: impl Into<String>, subject: &Subject) -> Value {
    json!({"reference":reference.into(),"subject":subject})
}
fn completed(outcome: &str, output: Value) -> DispatchResult {
    DispatchResult::Completed {
        outcome: outcome.into(),
        output,
    }
}
fn episode_key(partition: &str, key: &str) -> String {
    format!("{partition}/{key}")
}

impl FakeHost {
    pub fn grant_all(&mut self, actor: &str, visibility: &[&str]) {
        self.grants.insert(
            actor.into(),
            catalog::catalog().commands.keys().cloned().collect(),
        );
        self.visibility.insert(
            actor.into(),
            visibility.iter().map(|s| (*s).into()).collect(),
        );
    }
    fn visible(&self, actor: &str, visibility: &str) -> bool {
        self.visibility
            .get(actor)
            .is_some_and(|v| v.contains(visibility))
    }
    fn current(&self, subject: &Subject) -> bool {
        self.subjects.get(&subject.id) == Some(&subject.revision)
    }
    fn check_current(&self, subject: &Subject) -> Result<(), String> {
        if self.current(subject) {
            Ok(())
        } else {
            Err("subject revision conflict".into())
        }
    }
    pub fn check_key(subject: &Subject, suite: &str) -> String {
        format!("{}@{}/{suite}", subject.id, subject.revision)
    }
    pub fn restart(&self) -> Self {
        serde_json::from_slice(&serde_json::to_vec(self).expect("fake host JSON"))
            .expect("fake host restart")
    }

    /// Freeze observations and episode revisions before scanning. Pages never
    /// acquire authority merely because the workflow later sees another page.
    pub fn capture_observation(&mut self, observation: Observation) -> Result<(), String> {
        // This fake registers only the catalog's one project scope. A production
        // multi-scope host must namespace all domain keys by authenticated scope.
        if observation.scope != "project"
            || observation.partition.is_empty()
            || observation.partition.contains('/')
        {
            return Err("unsupported observation scope or partition".into());
        }
        if observation.pages.is_empty()
            || observation.pages.len() > 100
            || observation.expires_at_ms <= observation.observed_at_ms
        {
            return Err("invalid bounded observation".into());
        }
        if self.observations.contains_key(&observation.reference) {
            return Err("observation references are immutable".into());
        }
        let mut observation = observation;
        let prefix = format!("{}/", observation.partition);
        observation.episode_revisions = self
            .episodes
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(key, e)| (key.clone(), e.revision))
            .collect();
        self.observations
            .insert(observation.reference.clone(), observation);
        Ok(())
    }

    /// Record a trusted evaluator's exact bounded findings before a workflow
    /// asks for reconciliation. A caller-supplied evaluation string is not proof.
    pub fn record_evaluation(
        &mut self,
        reference: &str,
        observation: &str,
        findings: &Value,
    ) -> Result<(), String> {
        let scan = self
            .observations
            .get(observation)
            .ok_or("unknown evaluation observation")?;
        let items = findings
            .as_array()
            .ok_or("evaluation findings must be an array")?;
        if items.len() > 100 || self.evaluations.contains_key(reference) {
            return Err("evaluation bound or immutable identity conflict".into());
        }
        let evaluation = Evaluation {
            reference: reference.into(),
            observation: observation.into(),
            partition: scan.partition.clone(),
            scope: scan.scope.clone(),
            findings_digest: digest(findings).map_err(|e| e.to_string())?,
        };
        self.evaluations.insert(reference.into(), evaluation);
        Ok(())
    }

    /// Host notification service accepts each identity exactly once. A lost
    /// acknowledgment leaves the outbox pending and safely retries that identity.
    pub fn deliver_notifications(
        &mut self,
        limit: usize,
        acknowledge: bool,
    ) -> Result<usize, String> {
        if limit > 1000 {
            return Err("delivery bound exceeded".into());
        }
        let mut count = 0;
        for notification in self
            .outbox
            .values_mut()
            .filter(|n| !n.acknowledged)
            .take(limit)
        {
            if let Some(old) = self.recipient_receipts.get(&notification.identity) {
                if old != &notification.body {
                    return Err("notification identity content conflict".into());
                }
            } else {
                self.recipient_receipts
                    .insert(notification.identity.clone(), notification.body.clone());
            }
            notification.acknowledged = acknowledge;
            count += 1;
        }
        Ok(count)
    }

    pub fn complete_author_work(
        &mut self,
        key: &str,
        body: String,
        visibility: &str,
    ) -> Result<(), String> {
        let work = self.work.get_mut(key).ok_or("unknown author work")?;
        if work.kind != "supervisor_authoring" || work.visibility != visibility {
            return Err("author result visibility mismatch".into());
        }
        if let Some(old) = &work.authored_result
            && old != &body
        {
            return Err("author result immutable".into());
        }
        work.authored_result = Some(body);
        Ok(())
    }

    /// Explicit trusted domain completion. Creating work never supplies a review
    /// disposition. Tests must attest the exact result and business disposition.
    pub fn complete_work(
        &mut self,
        reference: &str,
        disposition: &str,
        result_reference: &str,
    ) -> Result<EventEnvelope, String> {
        if !matches!(disposition, "accepted" | "changes_requested" | "failed") {
            return Err("unsupported work disposition".into());
        }
        let work = self
            .work
            .values_mut()
            .find(|work| work.reference == reference)
            .ok_or("unknown host work handle")?;
        let output =
            json!({"reference":result_reference,"subject":work.subject,"disposition":disposition});
        if let Some(old) = &work.completion
            && old != &output
        {
            return Err("work completion is immutable".into());
        }
        work.completion = Some(output.clone());
        let subject = work.subject.clone();
        let completion = completed("completed", output.clone());
        for receipt in self.operations.values_mut() {
            if matches!(&receipt.result,DispatchResult::Accepted{handle} if handle==reference) {
                receipt.result = completion.clone();
            }
        }
        let contract = catalog::catalog().events["operation.changed"].clone();
        Ok(EventEnvelope {
            producer: "aq.operations".into(),
            event_id: format!("completion:{reference}"),
            contract: ContractRef {
                name: contract.name.clone(),
                fingerprint: contract.fingerprint().map_err(|e| e.to_string())?,
            },
            scope: "project".into(),
            correlation: BTreeMap::from([("operation".into(), reference.into())]),
            subject: Some(subject.id),
            revision: Some(subject.revision),
            causation: None,
            occurred_at_ms: Some(self.now_ms as i64),
            observed_at_ms: Some(self.now_ms as i64),
            payload: json!({"outcome":"completed","output":output}),
        })
    }

    /// Calendar calculation is host-owned. The fake accepts explicit instants and
    /// pins timezone/misfire/configuration metadata rather than doing DST math.
    pub fn admit_occurrence(
        &mut self,
        identity: &str,
        schedule: &str,
        at_ms: u64,
        expected_revision: u64,
    ) -> Result<CalendarOccurrence, String> {
        if let Some(old) = self.occurrences.get(identity) {
            if old.schedule != schedule || old.scheduled_at_ms != at_ms {
                return Err("calendar occurrence identity conflict".into());
            }
            return Ok(old.clone());
        }
        let config = self.calendars.get(schedule).ok_or("unknown calendar")?;
        if config.revision != expected_revision
            || config.timezone.is_empty()
            || !matches!(
                config.misfire.as_str(),
                "skip" | "coalesce" | "catch_up_bounded"
            )
        {
            return Err("calendar configuration conflict".into());
        }
        let occurrence = CalendarOccurrence {
            identity: identity.into(),
            schedule: schedule.into(),
            scheduled_at_ms: at_ms,
            timezone: config.timezone.clone(),
            misfire: config.misfire.clone(),
            configuration_revision: config.revision,
        };
        self.occurrences.insert(identity.into(), occurrence.clone());
        Ok(occurrence)
    }

    fn reconcile_findings(&mut self, intent: &Intent) -> Result<DispatchResult, String> {
        let partition = field(&intent.input, "partition")?;
        let observation = self
            .observations
            .get(field(&intent.input, "observation")?)
            .ok_or("unknown observation")?
            .clone();
        if observation.partition != partition
            || observation.scope != intent.authority.scope
            || !self.visible(&intent.authority.actor, &observation.visibility)
            || observation.expires_at_ms <= self.now_ms
            || !self.current(&observation.subject)
        {
            return Err("observation expired, inapplicable or invisible".into());
        }
        let evaluation = self
            .evaluations
            .get(field(&intent.input, "evaluation")?)
            .ok_or("unknown host evaluation")?;
        if evaluation.observation != observation.reference
            || evaluation.partition != partition
            || evaluation.scope != intent.authority.scope
            || evaluation.findings_digest
                != digest(&intent.input["findings"]).map_err(|e| e.to_string())?
        {
            return Err("evaluation does not attest these observation findings".into());
        }
        let findings = intent.input["findings"]
            .as_array()
            .ok_or("findings list required")?;
        if findings.len() > 100 {
            return Err("findings bound exceeded".into());
        }
        let reminder = intent.input["reminder_ms"]
            .as_u64()
            .ok_or("reminder required")?;
        let mut observed = BTreeMap::new();
        for finding in findings {
            let finding_key = field(finding, "key")?;
            if finding_key.is_empty() || finding_key.contains('/') {
                return Err("invalid finding key".into());
            }
            let key = episode_key(partition, finding_key);
            if observed
                .insert(key.clone(), field(finding, "evidence")?.to_string())
                .is_some()
            {
                return Err("duplicate finding identity".into());
            }
            let expected = finding["expected_revision"]
                .as_u64()
                .ok_or("expected episode revision required")?;
            if observation
                .episode_revisions
                .get(&key)
                .copied()
                .unwrap_or(0)
                != expected
            {
                return Err("finding revision is not the pre-scan revision".into());
            }
        }
        let prefix = format!("{partition}/");
        // Check every applicable baseline before mutating any episode, including
        // those absent from the findings (the ones a complete scan may clear).
        for (key, episode) in self.episodes.iter().filter(|(k, _)| k.starts_with(&prefix)) {
            if observation.episode_revisions.get(key).copied().unwrap_or(0) != episode.revision {
                return Err("stale pre-scan episode revision".into());
            }
        }
        for key in observed.keys() {
            if !self.episodes.contains_key(key) && observation.episode_revisions.contains_key(key) {
                return Err("missing retained episode".into());
            }
        }
        let mut keys: BTreeSet<_> = self
            .episodes
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .cloned()
            .collect();
        keys.extend(observed.keys().cloned());
        for key in keys {
            let next_evidence = observed.get(&key);
            let old = self.episodes.get(&key).cloned();
            let kind = match (&old, next_evidence) {
                (None, Some(_)) => Some("opened"),
                (Some(e), Some(v)) if !e.active || &e.evidence != v => Some("changed"),
                (Some(e), Some(_))
                    if reminder > 0
                        && self.now_ms.saturating_sub(e.last_notification_ms) >= reminder =>
                {
                    Some("reminder")
                }
                (Some(e), None) if e.active && observation.complete => Some("cleared"),
                _ => None,
            };
            if let Some(kind) = kind {
                let revision = old.as_ref().map_or(1, |e| e.revision + 1);
                let evidence = next_evidence
                    .cloned()
                    .or_else(|| old.as_ref().map(|e| e.evidence.clone()))
                    .unwrap_or_default();
                self.episodes.insert(
                    key.clone(),
                    Episode {
                        revision,
                        active: next_evidence.is_some(),
                        evidence: evidence.clone(),
                        last_notification_ms: self.now_ms,
                    },
                );
                let identity = format!("finding/{key}/{revision}/{kind}");
                self.outbox.insert(
                    identity.clone(),
                    Notification {
                        identity,
                        body: format!("{kind}: {key}: {evidence}"),
                        acknowledged: false,
                    },
                );
            }
        }
        Ok(completed(
            "reconciled",
            result(
                format!("evaluation:{}", field(&intent.input, "evaluation")?),
                &observation.subject,
            ),
        ))
    }

    fn execute(&mut self, intent: &Intent) -> Result<DispatchResult, String> {
        let input = &intent.input;
        let actor = &intent.authority.actor;
        match intent.command.name.as_str() {
            "context.read" => {
                let mut subject = subject_field(input, "subject")?;
                let requested_snapshot = field(input, "snapshot")?;
                let reference = if requested_snapshot.is_empty() {
                    if field(input, "view")? != "project"
                        || input["page"].as_u64() != Some(0)
                        || subject.id != "main"
                        || intent.authority.scope != "project"
                        || !self.visible(actor, "project")
                    {
                        return Err(
                            "fresh snapshots support only authorized project/main page zero".into(),
                        );
                    }
                    subject.revision = *self.subjects.get("main").ok_or("unknown base subject")?;
                    let reference = format!("base-snapshot:{}", intent.id);
                    let expires_at_ms = self
                        .now_ms
                        .checked_add(60_000)
                        .ok_or("observation expiry overflow")?;
                    self.capture_observation(Observation {
                        reference: reference.clone(),
                        partition: "base-main".into(),
                        scope: "project".into(),
                        visibility: "project".into(),
                        view: "project".into(),
                        subject: subject.clone(),
                        pages: vec![json!({"subject":subject})],
                        complete: true,
                        observed_at_ms: self.now_ms,
                        expires_at_ms,
                        episode_revisions: BTreeMap::new(),
                    })?;
                    reference
                } else {
                    requested_snapshot.into()
                };
                let observation = self
                    .observations
                    .get(&reference)
                    .ok_or("unknown frozen snapshot")?;
                if subject != observation.subject
                    || observation.scope != intent.authority.scope
                    || observation.view != field(input, "view")?
                    || !self.visible(actor, &observation.visibility)
                {
                    return Err("snapshot subject, view, scope or visibility mismatch".into());
                }
                let page = input["page"].as_u64().ok_or("page required")? as usize;
                if page >= observation.pages.len() {
                    return Err("page outside frozen snapshot".into());
                }
                let next = if page + 1 < observation.pages.len() {
                    Some(page + 1)
                } else {
                    None
                };
                let expired = self.now_ms >= observation.expires_at_ms;
                Ok(completed(
                    if expired {
                        "expired"
                    } else if !observation.complete || next.is_some() {
                        "incomplete"
                    } else {
                        "observed"
                    },
                    json!({"reference":format!("{}#{page}",observation.reference),"subject":subject,"snapshot":observation.reference,"complete":observation.complete&&next.is_none(),"next_page":next,"visibility":observation.visibility,"observed_at_ms":observation.observed_at_ms,"expires_at_ms":observation.expires_at_ms}),
                ))
            }
            "work.ensure" => {
                let subject = subject_field(input, "subject")?;
                self.check_current(&subject)?;
                let visibility = field(input, "visibility")?;
                if !self.visible(actor, visibility) {
                    return Err("work visibility refused".into());
                }
                let context = self
                    .observations
                    .get(field(input, "context")?)
                    .ok_or("unknown frozen work context")?;
                if context.visibility != visibility
                    || context.scope != intent.authority.scope
                    || context.subject != subject
                    || context.expires_at_ms <= self.now_ms
                {
                    return Err(
                        "work context scope, subject, freshness or visibility mismatch".into(),
                    );
                }
                let key = format!(
                    "{}/{}",
                    intent.authority.scope,
                    field(input, "logical_key")?
                );
                let request_digest = digest(input).map_err(|e| e.to_string())?;
                if let Some(old) = self.work.get(&key) {
                    if old.request_digest != request_digest {
                        return Err("logical work key content conflict".into());
                    }
                    return Ok(if old.lifetime == "owned" {
                        match &old.completion {
                            Some(output) => completed("completed", output.clone()),
                            None => DispatchResult::Accepted {
                                handle: old.reference.clone(),
                            },
                        }
                    } else {
                        completed("ensured", result(&old.reference, &old.subject))
                    });
                }
                let work = Work {
                    reference: format!("work:{}", intent.id),
                    subject: subject.clone(),
                    kind: field(input, "kind")?.into(),
                    visibility: visibility.into(),
                    request_digest,
                    authored_result: None,
                    lifetime: field(input, "lifetime")?.into(),
                    operation_id: intent.id.clone(),
                    completion: None,
                    cancelled: false,
                };
                let response = if work.lifetime == "owned" {
                    DispatchResult::Accepted {
                        handle: work.reference.clone(),
                    }
                } else {
                    completed("ensured", result(&work.reference, &subject))
                };
                self.work.insert(key, work);
                Ok(response)
            }
            "work.change" => {
                let subject = subject_field(input, "subject")?;
                if !self.current(&subject) {
                    return Ok(completed("conflict", result("stale-subject", &subject)));
                }
                match field(input, "variant")? {
                    "hold" => {
                        let id = input["hold_id"].as_str().ok_or("hold identity required")?;
                        let hold = Hold {
                            subject: subject.clone(),
                            actor: actor.clone(),
                            scope: intent.authority.scope.clone(),
                        };
                        if let Some(old) = self.holds.get(id)
                            && (old.actor != *actor
                                || old.scope != hold.scope
                                || old.subject != subject)
                        {
                            return Err("hold identity owned elsewhere".into());
                        }
                        self.holds.insert(id.into(), hold);
                    }
                    "release_hold" => {
                        let id = input["hold_id"].as_str().ok_or("hold identity required")?;
                        let hold = self.holds.get(id).ok_or("unknown hold")?;
                        if hold.actor != *actor
                            || hold.scope != intent.authority.scope
                            || hold.subject.id != subject.id
                        {
                            return Err("hold release authority refused".into());
                        }
                        self.holds.remove(id);
                    }
                    _ => {
                        self.subjects
                            .insert(subject.id.clone(), subject.revision + 1);
                    }
                }
                Ok(completed(
                    "changed",
                    result(
                        format!("change:{}", intent.id),
                        &Subject {
                            id: subject.id.clone(),
                            revision: self.subjects[&subject.id],
                        },
                    ),
                ))
            }
            "check.request" => {
                let subject = subject_field(input, "subject")?;
                self.check_current(&subject)?;
                let suite = field(input, "suite")?;
                let Some(outcome) = self
                    .configured_outcomes
                    .get(&Self::check_key(&subject, suite))
                    .cloned()
                else {
                    return Ok(DispatchResult::Unknown {
                        reason: "missing fake check outcome".into(),
                    });
                };
                if !matches!(
                    outcome.as_str(),
                    "passed" | "failed" | "infrastructure_failure" | "refused"
                ) {
                    return Err("invalid configured check outcome".into());
                }
                let reference = format!("check:{}", intent.id);
                self.evidence.insert(
                    reference.clone(),
                    Evidence {
                        subject: subject.clone(),
                        suite: suite.into(),
                        passed: outcome == "passed",
                        issuer: "synthetic-check-service".into(),
                    },
                );
                Ok(completed(&outcome, result(reference, &subject)))
            }
            "candidate.prepare" => {
                let base = subject_field(input, "base")?;
                let sources: Vec<Subject> = serde_json::from_value(input["sources"].clone())
                    .map_err(|_| "invalid candidate sources")?;
                if !self.current(&base) || sources.iter().any(|s| !self.current(s)) {
                    return Ok(completed(
                        "conflict",
                        result("stale-candidate-input", &base),
                    ));
                }
                let subject = Subject {
                    id: format!("candidate:{}", intent.id),
                    revision: 1,
                };
                self.subjects.insert(subject.id.clone(), 1);
                self.candidates.insert(
                    subject.id.clone(),
                    Candidate {
                        subject: subject.clone(),
                        base,
                        sources,
                    },
                );
                Ok(completed("prepared", result(&subject.id, &subject)))
            }
            "candidate.publish" => {
                let candidate = subject_field(input, "candidate")?;
                let destination = subject_field(input, "destination")?;
                let Some(record) = self.candidates.get(&candidate.id) else {
                    return Err("unknown immutable candidate".into());
                };
                if record.subject != candidate
                    || record.base != destination
                    || !self.current(&candidate)
                    || !self.current(&destination)
                {
                    return Ok(completed(
                        "conflict",
                        result("publication-precondition", &candidate),
                    ));
                }
                if self.holds.values().any(|h| {
                    h.scope == intent.authority.scope
                        && (h.subject.id == candidate.id || h.subject.id == destination.id)
                }) {
                    return Err("publication blocked by an active hold".into());
                }
                let evidence: Vec<Subject> = serde_json::from_value(input["evidence"].clone())
                    .map_err(|_| "invalid evidence subjects")?;
                if evidence.is_empty()
                    || evidence.iter().any(|e| e != &candidate)
                    || !self.evidence.values().any(|e| {
                        e.subject == candidate && e.passed && e.issuer == "synthetic-check-service"
                    })
                {
                    return Err(
                        "passing evidence for this exact candidate revision required".into(),
                    );
                }
                self.subjects
                    .insert(destination.id.clone(), destination.revision + 1);
                Ok(completed(
                    "published",
                    result(format!("publication:{}", intent.id), &candidate),
                ))
            }
            "message.send" => {
                let visibility = field(input, "visibility")?;
                if !self.visible(actor, visibility)
                    || !self.visible(field(input, "recipient")?, visibility)
                {
                    return Err("message sender or recipient visibility refused".into());
                }
                let identity = field(input, "identity")?;
                if let Some(old) = self.messages.get(identity)
                    && old != input
                {
                    return Err("message identity content conflict".into());
                }
                self.messages.insert(identity.into(), input.clone());
                Ok(completed(
                    "accepted",
                    result(
                        identity,
                        &Subject {
                            id: identity.into(),
                            revision: 1,
                        },
                    ),
                ))
            }
            "findings.reconcile" => self.reconcile_findings(intent),
            "judgment.request" | "decision.request" => {
                let subject = subject_field(input, "subject")?;
                self.check_current(&subject)?;
                if let Some(outcome) = self.configured_outcomes.get(&intent.id)
                    && intent.command.outcomes.contains_key(outcome)
                {
                    let mut output = result(format!("answer:{}", intent.id), &subject);
                    if intent.command.name == "judgment.request" && outcome == "assessed" {
                        let choice = self
                            .configured_choices
                            .get(&intent.id)
                            .ok_or("missing configured judgment choice")?;
                        if !input["choices"].as_array().is_some_and(|choices| {
                            choices.iter().any(|v| v.as_str() == Some(choice))
                        }) {
                            return Err("judgment choice outside authorized options".into());
                        }
                        output["choice"] = choice.clone().into();
                    }
                    return Ok(completed(outcome, output));
                }
                Ok(DispatchResult::Unknown {
                    reason: "missing fake authorized judgment/decision".into(),
                })
            }
            "operation.get" | "operation.cancel" => {
                let handle = field(input, "handle")?;
                let subject = Subject {
                    id: handle.into(),
                    revision: 1,
                };
                let operation = self
                    .operations
                    .get(handle)
                    .or_else(|| {
                        self.work
                            .values()
                            .find(|work| work.reference == handle)
                            .and_then(|work| self.operations.get(&work.operation_id))
                    })
                    .ok_or("unknown host operation")?;
                if operation.actor != *actor || operation.scope != intent.authority.scope {
                    return Err("operation authority refused".into());
                }
                let running = matches!(operation.result, DispatchResult::Accepted { .. });
                let work = self.work.values_mut().find(|work| work.reference == handle);
                let outcome = if intent.command.name == "operation.cancel" {
                    match work {
                        Some(work) if work.cancelled => "cancelled",
                        Some(work) if work.completion.is_none() => {
                            work.cancelled = true;
                            "cancelled"
                        }
                        _ => "too_late",
                    }
                } else {
                    match work {
                        Some(work) if work.cancelled => "cancelled",
                        Some(work)
                            if work
                                .completion
                                .as_ref()
                                .is_some_and(|output| output["disposition"] == "failed") =>
                        {
                            "failed"
                        }
                        _ if running => "running",
                        _ => "succeeded",
                    }
                };
                Ok(completed(outcome, result(handle, &subject)))
            }
            _ => Err("unregistered synthetic command".into()),
        }
    }
}

impl Host for FakeHost {
    fn authorize(&self, authority: &Authority, contract: &CommandContract) -> bool {
        let registry = catalog::catalog();
        registry.commands.get(&contract.name) == Some(contract)
            && contract.scopes.contains(&authority.scope)
            && contract.capabilities.is_subset(&authority.capabilities)
            && self
                .grants
                .get(&authority.actor)
                .is_some_and(|g| contract.capabilities.is_subset(g))
    }
    fn dispatch(&mut self, intent: &Intent) -> DispatchResult {
        if !self.authorize(&intent.authority, &intent.command) {
            return DispatchResult::Rejected {
                reason: "fake host authority refused".into(),
            };
        }
        if let Err(error) = intent.command.input.validate(&intent.input) {
            return DispatchResult::Rejected {
                reason: error.to_string(),
            };
        }
        let request_digest = match digest(&(&intent.command, &intent.input, &intent.authority)) {
            Ok(d) => d,
            Err(e) => {
                return DispatchResult::Rejected {
                    reason: e.to_string(),
                };
            }
        };
        if let Some(old) = self.operations.get(&intent.id) {
            return if old.request_digest == request_digest {
                old.result.clone()
            } else {
                DispatchResult::Rejected {
                    reason: "operation identity content conflict".into(),
                }
            };
        }
        // Clone/commit models a host-owned transaction, independent of the runner.
        let mut staged = self.clone();
        let response = match staged.execute(intent) {
            Ok(response) => response,
            Err(reason) => return DispatchResult::Rejected { reason },
        };
        if matches!(response, DispatchResult::Unknown { .. }) {
            return response;
        }
        if let DispatchResult::Completed { outcome, output } = &response {
            let valid = intent
                .command
                .outcomes
                .get(outcome)
                .is_some_and(|schema| schema.validate(output).is_ok());
            if !valid {
                return DispatchResult::Rejected {
                    reason: "fake host output violates captured contract".into(),
                };
            }
        }
        staged.operations.insert(
            intent.id.clone(),
            OperationReceipt {
                request_digest,
                actor: intent.authority.actor.clone(),
                scope: intent.authority.scope.clone(),
                result: response.clone(),
            },
        );
        let lost = staged.lose_next_response;
        staged.lose_next_response = false;
        *self = staged;
        if lost {
            DispatchResult::Unknown {
                reason: "synthetic transport interrupted after host commit".into(),
            }
        } else {
            response
        }
    }
    fn reconcile(&mut self, intent: &Intent) -> DispatchResult {
        self.dispatch(intent)
    }
    fn cancel(&mut self, intent: &Intent) -> CancellationResult {
        if !self.authorize(&intent.authority, &intent.command) {
            return CancellationResult::Refused;
        }
        let Some(work) = self
            .work
            .values_mut()
            .find(|work| work.operation_id == intent.id)
        else {
            return CancellationResult::Unknown;
        };
        if work.completion.is_some() {
            return CancellationResult::TooLate;
        }
        work.cancelled = true;
        CancellationResult::Confirmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_playbooks_runner::IntentStatus;
    fn subject() -> Subject {
        Subject {
            id: "project:sample".into(),
            revision: 1,
        }
    }
    fn fixture() -> FakeHost {
        let mut h = FakeHost {
            now_ms: 10,
            ..FakeHost::default()
        };
        h.subjects.insert(subject().id, 1);
        h.grant_all("alice", &["team"]);
        h.grant_all("bob", &["team"]);
        h.visibility
            .insert("supervisor".into(), BTreeSet::from(["team".into()]));
        h
    }
    fn intent(id: &str, name: &str, input: Value) -> Intent {
        Intent {
            id: id.into(),
            run: "run".into(),
            step: "step".into(),
            invocation: 1,
            deadline_at_ms: 60_000,
            command: catalog::catalog().commands[name].clone(),
            input_digest: digest(&input).unwrap(),
            input,
            authority: Authority {
                actor: "alice".into(),
                scope: "project".into(),
                capabilities: catalog::catalog().commands.keys().cloned().collect(),
            },
            status: IntentStatus::Prepared,
            cancel_requested: false,
            cancel_dispatched: false,
            late_results: vec![],
            completion_event: None,
            handle: None,
            result: None,
            reason: None,
        }
    }
    fn outcome(response: DispatchResult) -> (String, Value) {
        match response {
            DispatchResult::Completed { outcome, output } => (outcome, output),
            other => panic!("expected completion, got {other:?}"),
        }
    }
    fn observation(reference: &str, complete: bool, expires: u64) -> Observation {
        Observation {
            reference: reference.into(),
            partition: "watcher".into(),
            scope: "project".into(),
            visibility: "team".into(),
            view: "health_observations".into(),
            subject: subject(),
            pages: vec![json!({"subject":"project:sample","stuck":true})],
            complete,
            observed_at_ms: 1,
            expires_at_ms: expires,
            episode_revisions: BTreeMap::new(),
        }
    }
    fn reconcile(id: &str, reference: &str, findings: Value) -> Intent {
        intent(
            id,
            "findings.reconcile",
            json!({"partition":"watcher","observation":reference,"evaluation":id,"findings":findings,"reminder_ms":100}),
        )
    }
    fn evaluate(h: &mut FakeHost, id: &str, reference: &str, findings: Value) -> DispatchResult {
        h.record_evaluation(id, reference, &findings).unwrap();
        h.dispatch(&reconcile(id, reference, findings))
    }
    fn finding(revision: u64) -> Value {
        json!([{"key":"stuck","evidence":"diagnostic:revision1","expected_revision":revision}])
    }
    fn open_episode(h: &mut FakeHost) {
        h.capture_observation(observation("scan1", true, 1000))
            .unwrap();
        assert_eq!(
            outcome(evaluate(h, "open", "scan1", finding(0))).0,
            "reconciled"
        );
    }

    #[test]
    fn findings_suppress_unchanged_then_remind_and_clear_from_complete_fresh_baselines() {
        let mut h = fixture();
        open_episode(&mut h);
        assert_eq!(h.outbox.len(), 1);
        assert_eq!(h.episodes["watcher/stuck"].revision, 1);
        h.capture_observation(observation("scan2", true, 1000))
            .unwrap();
        evaluate(&mut h, "unchanged", "scan2", finding(1));
        assert_eq!(h.outbox.len(), 1);
        assert_eq!(h.episodes["watcher/stuck"].revision, 1);
        h.now_ms = 110;
        h.capture_observation(observation("scan3", true, 1000))
            .unwrap();
        evaluate(&mut h, "reminder", "scan3", finding(1));
        assert_eq!(h.outbox.len(), 2);
        assert_eq!(h.episodes["watcher/stuck"].revision, 2);
        h.capture_observation(observation("scan4", true, 1000))
            .unwrap();
        evaluate(&mut h, "clear", "scan4", json!([]));
        assert!(!h.episodes["watcher/stuck"].active);
        assert_eq!(h.outbox.len(), 3);
    }

    #[test]
    fn partial_expired_stale_or_invisible_observations_cannot_clear_findings() {
        let mut h = fixture();
        open_episode(&mut h);
        h.capture_observation(observation("partial", false, 1000))
            .unwrap();
        h.capture_observation(observation("expired", true, 5))
            .unwrap();
        let mut invisible = observation("invisible", true, 1000);
        invisible.visibility = "restricted".into();
        h.capture_observation(invisible).unwrap();
        assert_eq!(
            outcome(evaluate(&mut h, "partial", "partial", json!([]))).0,
            "reconciled"
        );
        assert!(h.episodes["watcher/stuck"].active);
        for reference in ["expired", "invisible"] {
            assert!(matches!(
                evaluate(&mut h, reference, reference, json!([])),
                DispatchResult::Rejected { .. }
            ));
            assert!(h.episodes["watcher/stuck"].active);
        }
        h.capture_observation(observation("stale", true, 1000))
            .unwrap();
        h.capture_observation(observation("changed", true, 1000))
            .unwrap();
        let changed =
            json!([{"key":"stuck","evidence":"diagnostic:revision2","expected_revision":1}]);
        evaluate(&mut h, "change", "changed", changed);
        assert!(matches!(
            evaluate(&mut h, "stale-clear", "stale", json!([])),
            DispatchResult::Rejected { .. }
        ));
        assert!(h.episodes["watcher/stuck"].active);
        assert_eq!(h.episodes["watcher/stuck"].revision, 2);
    }

    #[test]
    fn findings_host_commit_and_recipient_ack_crashes_keep_one_notification() {
        let mut h = fixture();
        h.capture_observation(observation("scan", true, 1000))
            .unwrap();
        let request = reconcile("evaluation", "scan", finding(0));
        h.record_evaluation("evaluation", "scan", &finding(0))
            .unwrap();
        h.lose_next_response = true;
        assert!(matches!(
            h.dispatch(&request),
            DispatchResult::Unknown { .. }
        ));
        assert_eq!(h.episodes.len(), 1);
        assert_eq!(h.outbox.len(), 1);
        h = h.restart();
        assert_eq!(outcome(h.reconcile(&request)).0, "reconciled");
        assert_eq!(h.outbox.len(), 1);
        h.deliver_notifications(1, false).unwrap();
        assert_eq!(h.recipient_receipts.len(), 1);
        assert!(!h.outbox.values().next().unwrap().acknowledged);
        h = h.restart();
        h.deliver_notifications(1, true).unwrap();
        h.deliver_notifications(1, true).unwrap();
        assert_eq!(h.recipient_receipts.len(), 1);
        assert!(h.outbox.values().next().unwrap().acknowledged);
    }

    #[test]
    fn partial_observation_can_add_positive_findings_without_clearing_unseen_episodes() {
        let mut h = fixture();
        open_episode(&mut h);
        h.capture_observation(observation("partial-positive", false, 1000))
            .unwrap();
        let findings =
            json!([{"key":"new-alert","evidence":"observed:positive","expected_revision":0}]);
        assert_eq!(
            outcome(evaluate(
                &mut h,
                "partial-evaluation",
                "partial-positive",
                findings
            ))
            .0,
            "reconciled"
        );
        assert!(h.episodes["watcher/new-alert"].active);
        assert!(h.episodes["watcher/stuck"].active);
        assert_eq!(h.outbox.len(), 2);
    }

    #[test]
    fn findings_require_exact_host_evaluation_and_fake_refuses_other_scopes() {
        let mut h = fixture();
        h.capture_observation(observation("scan", true, 1000))
            .unwrap();
        let request = reconcile("evaluation", "scan", finding(0));
        assert!(matches!(
            h.dispatch(&request),
            DispatchResult::Rejected { .. }
        ));
        assert!(h.episodes.is_empty());
        h.record_evaluation("evaluation", "scan", &json!([]))
            .unwrap();
        assert!(matches!(
            h.dispatch(&request),
            DispatchResult::Rejected { .. }
        ));
        assert!(h.episodes.is_empty());
        let mut other = observation("other-scope", true, 1000);
        other.scope = "other-project".into();
        assert!(h.capture_observation(other).is_err());
        let mut forged = request;
        forged.authority.scope = "other-project".into();
        assert!(matches!(
            h.dispatch(&forged),
            DispatchResult::Rejected { .. }
        ));
        h.capture_observation(observation("another-scan", true, 1000))
            .unwrap();
        h.record_evaluation("other-evaluation", "another-scan", &finding(0))
            .unwrap();
        let mut mismatch = reconcile("another-operation", "scan", finding(0));
        mismatch.input["evaluation"] = "other-evaluation".into();
        assert!(matches!(
            h.dispatch(&mismatch),
            DispatchResult::Rejected { .. }
        ));
        assert!(h.episodes.is_empty());
    }

    fn prepare(h: &mut FakeHost) -> Subject {
        let response = h.dispatch(&intent(
            "prepare",
            "candidate.prepare",
            json!({"sources":[subject()],"base":subject()}),
        ));
        let (_, output) = outcome(response);
        parse_subject(&output["subject"]).unwrap()
    }
    fn check(h: &mut FakeHost, candidate: &Subject) {
        h.configured_outcomes
            .insert(FakeHost::check_key(candidate, "unit"), "passed".into());
        assert_eq!(
            outcome(h.dispatch(&intent(
                "check",
                "check.request",
                json!({"subject":candidate,"suite":"unit","execution_context":"synthetic"})
            )))
            .0,
            "passed"
        );
    }
    fn publication(id: &str, candidate: &Subject, evidence: &Subject) -> Intent {
        intent(
            id,
            "candidate.publish",
            json!({"candidate":candidate,"destination":subject(),"evidence":[evidence]}),
        )
    }
    fn hold(id: &str, variant: &str, hold_id: &str) -> Intent {
        intent(
            id,
            "work.change",
            json!({"variant":variant,"subject":subject(),"value":"publication hold","hold_id":hold_id}),
        )
    }

    #[test]
    fn publication_requires_exact_revision_evidence_and_current_destination() {
        let mut h = fixture();
        let candidate = prepare(&mut h);
        check(&mut h, &candidate);
        assert!(matches!(
            h.dispatch(&publication("wrong-subject", &candidate, &subject())),
            DispatchResult::Rejected { .. }
        ));
        let wrong_revision = Subject {
            id: candidate.id.clone(),
            revision: 2,
        };
        assert!(
            matches!(h.dispatch(&publication("wrong-revision",&wrong_revision,&wrong_revision)),DispatchResult::Completed{outcome,..}if outcome=="conflict")
        );
        h.subjects.insert(subject().id, 2);
        assert_eq!(
            outcome(h.dispatch(&publication("stale-destination", &candidate, &candidate))).0,
            "conflict"
        );
        assert_eq!(h.subjects[&subject().id], 2);
    }

    #[test]
    fn competing_holds_require_each_owners_release_and_publication_reconciles_once() {
        let mut h = fixture();
        let candidate = prepare(&mut h);
        check(&mut h, &candidate);
        h.dispatch(&hold("hold-a", "hold", "a"));
        let mut bob = hold("hold-b", "hold", "b");
        bob.authority.actor = "bob".into();
        h.dispatch(&bob);
        assert!(matches!(
            h.dispatch(&hold("forged-release", "release_hold", "b")),
            DispatchResult::Rejected { .. }
        ));
        h.dispatch(&hold("release-a", "release_hold", "a"));
        assert!(matches!(
            h.dispatch(&publication("held", &candidate, &candidate)),
            DispatchResult::Rejected { .. }
        ));
        let mut release_b = hold("release-b", "release_hold", "b");
        release_b.authority.actor = "bob".into();
        h.dispatch(&release_b);
        let publish = publication("publish", &candidate, &candidate);
        h.lose_next_response = true;
        assert!(matches!(
            h.dispatch(&publish),
            DispatchResult::Unknown { .. }
        ));
        assert_eq!(h.subjects[&subject().id], 2);
        h = h.restart();
        assert_eq!(outcome(h.reconcile(&publish)).0, "published");
        assert_eq!(h.subjects[&subject().id], 2);
    }

    #[test]
    fn finite_frozen_context_pages_and_report_work_respect_visibility() {
        let mut h = fixture();
        let mut scan = observation("report-context", true, 1000);
        scan.view = "project".into();
        scan.pages.push(json!({"second":"page"}));
        h.capture_observation(scan).unwrap();
        let context = |id: &str, page: u64| {
            intent(
                id,
                "context.read",
                json!({"view":"project","subject":subject(),"snapshot":"report-context","page":page}),
            )
        };
        let (first, output) = outcome(h.dispatch(&context("page0", 0)));
        assert_eq!(first, "incomplete");
        assert_eq!(output["next_page"], 1);
        assert_eq!(outcome(h.dispatch(&context("page1", 1))).0, "observed");
        assert!(matches!(
            h.dispatch(&context("outside", 2)),
            DispatchResult::Rejected { .. }
        ));
        let mut forbidden = context("forbidden", 0);
        forbidden.authority.actor = "outsider".into();
        h.grants
            .insert("outsider".into(), h.grants["alice"].clone());
        assert!(matches!(
            h.dispatch(&forbidden),
            DispatchResult::Rejected { .. }
        ));
        let work = intent(
            "author",
            "work.ensure",
            json!({"kind":"supervisor_authoring","subject":subject(),"logical_key":"report:2026-10-30","lifetime":"owned","context":"report-context","visibility":"team"}),
        );
        h.dispatch(&work);
        h = h.restart();
        h.dispatch(&work);
        assert_eq!(h.work.len(), 1);
        assert!(
            h.complete_author_work(
                "project/report:2026-10-30",
                "restricted material".into(),
                "restricted"
            )
            .is_err()
        );
        h.complete_author_work(
            "project/report:2026-10-30",
            "permitted report".into(),
            "team",
        )
        .unwrap();
        let message = intent(
            "fallback",
            "message.send",
            json!({"identity":"report:2026-10-30","recipient":"supervisor","visibility":"team","body":"fallback"}),
        );
        h.dispatch(&message);
        let mut late = message.clone();
        late.id = "late-authored".into();
        late.input["body"] = "late authored report".into();
        assert!(matches!(h.dispatch(&late), DispatchResult::Rejected { .. }));
        assert_eq!(h.messages.len(), 1);
        assert_eq!(h.messages["report:2026-10-30"]["body"], "fallback");
    }

    #[test]
    fn missing_fake_checks_and_decisions_remain_unknown_and_revocation_prevents_replay() {
        let mut h = fixture();
        let check = intent(
            "unconfigured",
            "check.request",
            json!({"subject":subject(),"suite":"unit","execution_context":"synthetic"}),
        );
        assert!(matches!(h.dispatch(&check), DispatchResult::Unknown { .. }));
        assert!(h.evidence.is_empty());
        let decision = intent(
            "decision",
            "decision.request",
            json!({"subject":subject(),"question":"approve?","choices":["approve","reject"],"responders":["alice"],"expires_at_ms":1000}),
        );
        assert!(matches!(
            h.dispatch(&decision),
            DispatchResult::Unknown { .. }
        ));
        h.configured_outcomes
            .insert("decision".into(), "approved".into());
        assert_eq!(outcome(h.dispatch(&decision)).0, "approved");
        h.grants.clear();
        assert!(matches!(
            h.reconcile(&decision),
            DispatchResult::Rejected { .. }
        ));
    }

    #[test]
    fn host_calendar_occurrences_pin_timezone_misfire_and_configuration_across_restart() {
        let mut h = fixture();
        h.calendars.insert(
            "morning-report".into(),
            CalendarConfiguration {
                revision: 7,
                timezone: "America/Los_Angeles".into(),
                misfire: "coalesce".into(),
            },
        );
        let occurrence = h
            .admit_occurrence("report-2026-11-01", "morning-report", 1793523600000, 7)
            .unwrap();
        h = h.restart();
        h.calendars.get_mut("morning-report").unwrap().revision = 8;
        h.calendars.get_mut("morning-report").unwrap().timezone = "UTC".into();
        assert_eq!(
            h.admit_occurrence("report-2026-11-01", "morning-report", 1793523600000, 8)
                .unwrap(),
            occurrence
        );
        assert_eq!(occurrence.configuration_revision, 7);
        assert_eq!(occurrence.timezone, "America/Los_Angeles");
        assert_eq!(occurrence.misfire, "coalesce");
        assert!(
            h.admit_occurrence("new-occurrence", "morning-report", 1793610000000, 7)
                .is_err()
        );
        assert!(
            h.admit_occurrence("report-2026-11-01", "morning-report", 1793610000000, 8)
                .is_err()
        );
    }

    #[test]
    fn fresh_base_reads_capture_authoritative_revision_and_preserve_named_snapshot() {
        let mut h = fixture();
        h.subjects.insert("main".into(), 7);
        h.visibility
            .get_mut("alice")
            .unwrap()
            .insert("project".into());
        let request = |id: &str| {
            intent(
                id,
                "context.read",
                json!({"view":"project","subject":{"id":"main","revision":1},"snapshot":"","page":0}),
            )
        };
        let first = request("base1");
        let (_, first_output) = outcome(h.dispatch(&first));
        assert_eq!(first_output["subject"]["revision"], 7);
        h.subjects.insert("main".into(), 8);
        assert_eq!(outcome(h.dispatch(&first)).1, first_output);
        let (_, second_output) = outcome(h.dispatch(&request("base2")));
        assert_eq!(second_output["subject"]["revision"], 8);
        assert_ne!(first_output["snapshot"], second_output["snapshot"]);
        let named = intent(
            "retained",
            "context.read",
            json!({"view":"project","subject":{"id":"main","revision":7},"snapshot":first_output["snapshot"],"page":0}),
        );
        assert_eq!(outcome(h.dispatch(&named)).1["subject"]["revision"], 7);
        for (id, key, value) in [
            ("bad-view", "view", json!("evidence")),
            ("bad-page", "page", json!(1)),
            ("bad-subject", "subject", json!({"id":"other","revision":1})),
        ] {
            let mut bad = request(id);
            bad.input[key] = value;
            assert!(matches!(h.dispatch(&bad), DispatchResult::Rejected { .. }));
        }
        let mut invisible = request("invisible");
        invisible.authority.actor = "bob".into();
        assert!(matches!(
            h.dispatch(&invisible),
            DispatchResult::Rejected { .. }
        ));
        assert_eq!(h.observations.len(), 2);
    }
}

//! Synthetic domain services for contract tests. This is deliberately not an AQ
//! adapter. Domain records and notification intents belong to this host, separate
//! from runner state; serializing `FakeHost` simulates a durable host restart.
//! Named `context.read` snapshots are immutable retained observations. The one
//! supported fresh-read mode uses `snapshot=""`, `view="project"`, `page=0`, and
//! subject id `main`; its input revision is a request hint, while the returned
//! subject revision comes from the current host record. Every fresh read records
//! its own bounded immutable snapshot and requires project visibility.
use aq_playbooks_cli::catalog;
use aq_playbooks_model::{CommandContract, ContractRef, EventEnvelope, Registry, digest};
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
pub struct Decision {
    pub subject: Subject,
    pub responders: BTreeSet<String>,
    pub expires_at_ms: u64,
    pub actor: String,
    pub scope: String,
    pub completion: Option<(String, Value)>,
    pub completed_at_ms: Option<u64>,
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
    #[serde(default)]
    pub decisions: BTreeMap<String, Decision>,
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
pub fn parse_subject(value: &Value) -> Result<Subject, String> {
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
        let contract = catalog::catalog().events["aq.operation.updated"].clone();
        Ok(EventEnvelope {
            producer: "aq.host.bridge@1".into(),
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

    /// The responder is an independently authenticated principal, never an
    /// approval flag supplied by the requesting workflow.
    pub fn complete_decision(
        &mut self,
        handle: &str,
        responder: &str,
        outcome: &str,
    ) -> Result<EventEnvelope, String> {
        let decision = self.decisions.get(handle).ok_or("unknown decision")?;
        if !decision.responders.contains(responder) || !matches!(outcome, "approved" | "rejected") {
            return Err("decision responder or outcome refused".into());
        }
        if decision.completion.is_none() {
            self.check_current(&decision.subject)?;
            if self.now_ms >= decision.expires_at_ms {
                return Err("decision deadline elapsed".into());
            }
        }
        self.settle_decision(handle, outcome)
    }

    fn settle_decision(&mut self, handle: &str, outcome: &str) -> Result<EventEnvelope, String> {
        let decision = self.decisions.get_mut(handle).ok_or("unknown decision")?;
        let output = result(format!("decision:{handle}"), &decision.subject);
        if let Some((old, _)) = &decision.completion {
            if old != outcome {
                return Err("decision completion is immutable".into());
            }
        } else {
            decision.completion = Some((outcome.into(), output.clone()));
            decision.completed_at_ms = Some(self.now_ms);
        }
        if let Some(receipt) = self.operations.get_mut(handle) {
            receipt.result = completed(outcome, output.clone());
        }
        let contract = &catalog::catalog().events["aq.decision.completed"];
        Ok(EventEnvelope {
            producer: "aq.host.bridge@1".into(),
            event_id: format!("decision-completion:{handle}"),
            contract: ContractRef {
                name: contract.name.clone(),
                fingerprint: contract.fingerprint().map_err(|e| e.to_string())?,
            },
            scope: decision.scope.clone(),
            correlation: BTreeMap::from([("operation".into(), handle.into())]),
            subject: Some(decision.subject.id.clone()),
            revision: Some(decision.subject.revision),
            causation: Some(handle.into()),
            occurred_at_ms: decision.completed_at_ms.map(|at| at as i64),
            observed_at_ms: decision.completed_at_ms.map(|at| at as i64),
            payload: json!({"outcome":outcome,"output":output}),
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
        if intent.command.version == catalog::VERSION_2
            && matches!(
                intent.command.name.as_str(),
                "judgment.request" | "check.request"
            )
        {
            // 2.0.0 admission only: the durable handle completes by host event.
            self.check_current(&subject_field(input, "subject")?)?;
            return Ok(DispatchResult::Accepted {
                handle: intent.id.clone(),
            });
        }
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
                let mut output = result(&subject.id, &subject);
                if intent.command.version == catalog::VERSION_2_1 {
                    // 2.1.0 returns the immutable candidate OID that checks bind.
                    let hex = aq_playbooks_model::hash_bytes(subject.id.as_bytes());
                    output["source"] =
                        json!({"format": "sha256", "hex": hex.trim_start_matches("sha256:")});
                }
                Ok(completed("prepared", output))
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
                    "queued",
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
            "decision.request" => {
                let subject = subject_field(input, "subject")?;
                self.check_current(&subject)?;
                let expires_at_ms = input["expires_at_ms"].as_u64().ok_or("invalid expiry")?;
                let responders: BTreeSet<String> =
                    serde_json::from_value(input["responders"].clone())
                        .map_err(|_| "invalid responders")?;
                if responders.is_empty()
                    || expires_at_ms <= self.now_ms
                    || input["choices"]
                        .as_array()
                        .is_none_or(|choices| choices.is_empty())
                {
                    return Err(
                        "decision needs eligible responders, choices and a future expiry".into(),
                    );
                }
                self.decisions.insert(
                    intent.id.clone(),
                    Decision {
                        subject,
                        responders,
                        expires_at_ms,
                        actor: actor.clone(),
                        scope: intent.authority.scope.clone(),
                        completion: None,
                        completed_at_ms: None,
                    },
                );
                Ok(DispatchResult::Accepted {
                    handle: intent.id.clone(),
                })
            }
            "judgment.request" => {
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
                let operation_id = self
                    .work
                    .values()
                    .find(|work| work.reference == handle)
                    .map(|work| work.operation_id.clone())
                    .unwrap_or_else(|| handle.into());
                let operation = self
                    .operations
                    .get(&operation_id)
                    .ok_or("unknown host operation")?;
                if operation.actor != *actor || operation.scope != intent.authority.scope {
                    return Err("operation authority refused".into());
                }
                if let Some(decision) = self.decisions.get(handle) {
                    if decision.completion.is_none() && self.now_ms >= decision.expires_at_ms {
                        self.settle_decision(handle, "expired")?;
                    }
                    if intent.command.name == "operation.cancel" {
                        if self.decisions[handle].completion.is_some() {
                            return Ok(completed("too_late", result(handle, &subject)));
                        }
                        self.settle_decision(handle, "cancelled")?;
                        return Ok(completed("cancelled", result(handle, &subject)));
                    }
                }
                let work = self.work.values_mut().find(|work| work.reference == handle);
                if intent.command.name == "operation.cancel" {
                    let outcome = match work {
                        Some(work) if work.cancelled => "cancelled",
                        Some(work) if work.completion.is_none() => {
                            work.cancelled = true;
                            "cancelled"
                        }
                        _ => "too_late",
                    };
                    return Ok(completed(outcome, result(handle, &subject)));
                }
                if work.is_some_and(|work| work.cancelled && work.completion.is_none()) {
                    return Ok(completed("cancelled", result(handle, &subject)));
                }
                Ok(match &self.operations[&operation_id].result {
                    DispatchResult::Accepted { .. } => {
                        completed("running", result(handle, &subject))
                    }
                    DispatchResult::Completed { outcome, output } => {
                        completed("completed", json!({"outcome":outcome,"output":output}))
                    }
                    DispatchResult::Unknown { .. } => {
                        completed("indeterminate", result(handle, &subject))
                    }
                    DispatchResult::Rejected { .. } => {
                        completed("refused", result(handle, &subject))
                    }
                })
            }
            _ => Err("unregistered synthetic command".into()),
        }
    }
}

impl Host for FakeHost {
    fn authorize(&self, authority: &Authority, contract: &CommandContract) -> bool {
        let published =
            |registry: Registry| registry.commands.get(&contract.name) == Some(contract);
        (published(catalog::catalog())
            || published(catalog::catalog_v2())
            || published(catalog::catalog_v2_1()))
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
        if let Some(decision) = self.decisions.get(&intent.id) {
            if decision.completion.is_some() {
                return CancellationResult::TooLate;
            }
            let outcome = if self.now_ms >= decision.expires_at_ms {
                "expired"
            } else {
                "cancelled"
            };
            return match self.settle_decision(&intent.id, outcome) {
                Ok(_) if outcome == "cancelled" => CancellationResult::Confirmed,
                Ok(_) => CancellationResult::TooLate,
                Err(_) => CancellationResult::Unknown,
            };
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

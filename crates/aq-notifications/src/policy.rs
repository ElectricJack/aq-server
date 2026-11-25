//! Attention episodes produced for automatic owner notifications.
use aq_lifecycle_api::Scope;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MILESTONE_GATE_LABEL: &str = "aq:milestone-gate";
const TITLE_CHARS: usize = 240;
const SUBJECT_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    TaskFailedOrBlocked,
    DecisionNeeded,
    /// Event kind defined here and emitted by F5 when an agent asks the owner.
    QuestionAsked,
    /// Attempt failure reason `credential_invalid` (C6b); subject = profile ID.
    CredentialInvalid,
    /// Subject = node ID of a node labelled [`MILESTONE_GATE_LABEL`].
    MilestoneGateReady,
    /// Subject = worker/node ID from worker presence.
    WorkerOffline,
    /// SBX25: subject = node ID or `central`; opens at 80% used, resolves
    /// below 75%.
    StoragePressure,
    /// SBX22(a): subject = attempt ID of a sandbox preserved on its node.
    SandboxPreserved,
    /// H3 stall ladder rung 3: subject = attempt ID; opened once per episode.
    SessionStalled,
}
impl Kind {
    pub const ALL: [Kind; 9] = [
        Kind::TaskFailedOrBlocked,
        Kind::DecisionNeeded,
        Kind::QuestionAsked,
        Kind::CredentialInvalid,
        Kind::MilestoneGateReady,
        Kind::WorkerOffline,
        Kind::StoragePressure,
        Kind::SandboxPreserved,
        Kind::SessionStalled,
    ];
    fn description(self) -> (&'static str, &'static str) {
        match self {
            Self::TaskFailedOrBlocked => ("task.failed_or_blocked", "Task failed or blocked"),
            Self::DecisionNeeded => ("decision.needed", "Decision needed"),
            Self::QuestionAsked => ("question.asked", "Question asked"),
            Self::CredentialInvalid => ("credential.invalid", "Credential invalid"),
            Self::MilestoneGateReady => ("milestone.gate_ready", "Milestone gate ready"),
            Self::WorkerOffline => ("worker.offline", "Worker offline"),
            Self::StoragePressure => ("storage.pressure", "Storage pressure"),
            Self::SandboxPreserved => ("sandbox.preserved", "Sandbox preserved"),
            Self::SessionStalled => ("session.stalled", "Session stalled"),
        }
    }
    pub fn as_str(self) -> &'static str {
        self.description().0
    }
    pub fn label(self) -> &'static str {
        self.description().1
    }
    /// Maps an attempt failure reason to the episode kind it opens, if any.
    pub fn from_failure_reason(reason: &str) -> Option<Kind> {
        (reason == "credential_invalid").then_some(Kind::CredentialInvalid)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transition {
    Opened,
    Resolved,
}

/// One confirmed source observation. `title` is a short label chosen by the
/// producer (never a transcript); it is sanitised and truncated here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub source_event_id: Uuid,
    pub kind: Kind,
    pub subject: String,
    pub scope: Scope,
    pub transition: Transition,
    pub title: String,
}

impl Event {
    pub fn subject_key(&self) -> Option<String> {
        let subject = sanitize(&self.subject, SUBJECT_BYTES);
        let identity = !self.source_event_id.is_nil()
            && !self.scope.project_id.is_nil()
            && !self.scope.workspace_id.is_nil();
        (identity && (1..=SUBJECT_BYTES).contains(&subject.len())).then_some(subject)
    }
    pub fn display_title(&self) -> String {
        sanitize(&self.title, TITLE_CHARS)
    }
}
fn sanitize(text: &str, max_chars: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = flat.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let mut cut: String = trimmed.chars().take(max_chars - 1).collect();
        cut.push('…');
        cut
    }
}

#[derive(Clone)]
pub struct Origin {
    pub id: Uuid,
    pub scope: Scope,
}
impl Origin {
    pub fn new(id: Uuid, scope: Scope) -> Self {
        Self { id, scope }
    }
    pub fn event(self, kind: Kind, subject: String, open: bool) -> Event {
        Event {
            source_event_id: derived_id(&[
                "aq-source/1",
                &self.id.to_string(),
                kind.as_str(),
                &self.scope.project_id.to_string(),
                &self.scope.workspace_id.to_string(),
                &subject,
                if open { "opened" } else { "resolved" },
            ]),
            kind,
            subject,
            scope: self.scope,
            transition: if open {
                Transition::Opened
            } else {
                Transition::Resolved
            },
            title: "Open AQ to review".into(),
        }
    }
    pub fn identified(self, kind: Kind, subject: Uuid, open: bool) -> crate::Result<Event> {
        if self.id.is_nil() || subject.is_nil() {
            return Err(crate::Error::Invalid);
        }
        Ok(self.event(kind, subject.to_string(), open))
    }
}
pub fn derived_id(parts: &[&str]) -> Uuid {
    let digest = aq_lifecycle_api::Digest::of(parts.join("\u{1f}").as_bytes());
    let hex = digest.as_str();
    let mut bytes = [0u8; 16];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
    }
    // RFC 9562 version 8 (custom) layout with the RFC variant bits.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

impl Origin {
    /// Drop already open ready gates and resolve subjects missing from the current projection.
    pub fn gate_transitions(&self, mut events: Vec<Event>, open: Vec<String>) -> Vec<Event> {
        let ready_subjects: std::collections::HashSet<_> =
            events.iter().map(|e| e.subject.clone()).collect();
        // Unchanged ready gates need no new policy receipt on every
        // Graph operation. Only transitions consume retention capacity.
        events.retain(|e| !open.contains(&e.subject));
        for subject in open {
            if !ready_subjects.contains(&subject) {
                events.push(self.clone().event(Kind::MilestoneGateReady, subject, false));
            }
        }
        events
    }
}

pub const NOTIFICATION_TTL_MS: u64 = 86_400_000;

pub struct Episode<'a> {
    pub event: &'a Event,
    pub subject: &'a str,
}
impl Episode<'_> {
    pub fn key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.event.kind.as_str(),
            self.event.scope.project_id,
            self.subject
        )
    }
    pub fn message(&self, recipient: Uuid, identity: Uuid, generation: i64) -> crate::SendRequest {
        let generation_text = generation.to_string();
        let key = &self.key();
        let event = self.event;
        let subject = self.subject;
        let summary = format!(
            "AQ attention: {} - {} ({})",
            event.kind.label(),
            event.display_title(),
            subject
        );
        crate::SendRequest {
            operation_id: derived_id(&[
                "aq-notify-op/2",
                key,
                &identity.to_string(),
                &generation_text,
            ]),
            message_id: derived_id(&[
                "aq-notify-msg/2",
                key,
                &identity.to_string(),
                &generation_text,
            ]),
            revision: 1,
            scope: event.scope.clone(),
            recipient_user_id: recipient,
            summary,
            ttl_ms: NOTIFICATION_TTL_MS,
        }
    }
}

pub struct FloodWindow {
    pub scope: Scope,
    pub start: i64,
}
impl FloodWindow {
    pub fn summary(&self, recipient: Uuid, collapsed: i32, kinds: &[String]) -> crate::SendRequest {
        let start_text = self.start.to_string();
        let scope = self.scope.project_id.to_string();
        crate::SendRequest {
            operation_id: derived_id(&["aq-flood-op/1", &scope, &start_text]),
            message_id: derived_id(&["aq-flood-msg/1", &scope, &start_text]),
            revision: 1,
            scope: self.scope.clone(),
            recipient_user_id: recipient,
            summary: format!(
                "AQ attention: {collapsed} more events need attention in one minute ({}). Open AQ to review.",
                kinds.join(", ")
            ),
            ttl_ms: NOTIFICATION_TTL_MS,
        }
    }
}

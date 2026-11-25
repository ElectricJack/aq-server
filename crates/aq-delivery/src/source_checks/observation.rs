//! Strict projection of the original Git observations retained for a profiled offer.
use super::{Repository, SourceHead, commit_matches, resolve_ref, retention_holds};
use aq_admission::{ArtifactRef, Digest, ObjectFormat, SourceAdmission, SourceSelection, Text};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    schema_version: u32,
    admission: SourceAdmission,
    retention_handle: Uuid,
    evidence: Evidence,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reference_observation: Option<Value>,
    /// D1: the server-owned source head AQ published to the named ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    publication_head: Option<Value>,
    /// Spec 0007: the handoff whose saved commit this immutable source is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handoff: Option<Value>,
    /// Spec 0016 §4: the synced, retained head of the followed named ref.
    #[cfg(feature = "delivery_playbook")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    main_sync: Option<Value>,
    commit_inspection: Value,
    retention_inspection: Value,
}

pub struct VerifiedObservation {
    pub admission: SourceAdmission,
    pub retention_handle: Uuid,
}
pub fn verify(bytes: &[u8], project: Uuid, now: u64) -> Option<VerifiedObservation> {
    let bounded = (1..=1_048_576).contains(&bytes.len());
    let scoped = !project.is_nil() && now > 0;
    if !bounded || !scoped {
        return None;
    }
    let body: Body = serde_json::from_slice(bytes).ok()?;
    body.admission.validate().ok()?;
    if !body.matches_project(project, now) || !body.matches_commit() {
        return None;
    }
    if !body.matches_selection(now) || !body.retained() {
        return None;
    }
    Some(VerifiedObservation {
        admission: body.admission,
        retention_handle: body.retention_handle,
    })
}
pub fn verify_stored(bytes: &[u8], project: Uuid) -> Option<VerifiedObservation> {
    if bytes.len() > 1_048_576 {
        return None;
    }
    let body: Body = serde_json::from_slice(bytes).ok()?;
    verify(bytes, project, body.admission.expires_at_ms.checked_sub(1)?)
}
impl Body {
    fn matches_project(&self, project: Uuid, now: u64) -> bool {
        let a = &self.admission;
        let schema = self.schema_version == 1 && a.resolved_object.format == ObjectFormat::Sha1;
        let scope = a.project_id == project && !self.retention_handle.is_nil();
        let current = (a.observed_at_ms..a.expires_at_ms).contains(&now);
        schema && scope && current
    }
    fn matches_commit(&self) -> bool {
        commit_matches(
            &self.evidence.commit_inspection,
            &self.admission.resolved_object.hex,
        )
    }
    fn retained(&self) -> bool {
        let a = &self.admission;
        let repository = Repository {
            id: a.repository_id,
            generation: a.repository_binding_generation,
        };
        retention_holds(
            &self.evidence.retention_inspection,
            self.retention_handle,
            repository,
            &a.resolved_object.hex,
        )
    }
    fn matches_selection(&self, now: u64) -> bool {
        match &self.admission.selection {
            SourceSelection::Immutable { object } => {
                #[cfg(feature = "delivery_playbook")]
                if self.evidence.main_sync.is_some() {
                    return false;
                }
                let standalone = self.evidence.reference_observation.is_none()
                    && self.evidence.publication_head.is_none();
                standalone && *object == self.admission.resolved_object
            }
            SourceSelection::NamedRef { reference } => {
                self.evidence.handoff.is_none() && self.matches_ref(reference.as_str(), now)
            }
        }
    }
    fn matches_ref(&self, reference: &str, now: u64) -> bool {
        let a = &self.admission;
        let e = &self.evidence;
        #[cfg(feature = "delivery_playbook")]
        if e.main_sync.is_some() {
            return self.synced_ref(reference);
        }
        if let Some(value) = &e.publication_head {
            return self.published_ref(value, reference);
        }
        let Some(record) = e.reference_observation.as_ref() else {
            return false;
        };
        resolve_ref(record, reference, a.repository_binding_generation, now).as_deref()
            == Some(a.resolved_object.hex.as_str())
    }
    fn published_ref(&self, value: &Value, reference: &str) -> bool {
        let Ok(head) = serde_json::from_value::<SourceHead>(value.clone()) else {
            return false;
        };
        let a = &self.admission;
        let scope = (
            a.project_id,
            a.repository_id,
            a.repository_binding_generation,
        );
        let supplied = (head.project_id, head.repository_id, head.binding_generation);
        let identity = (
            reference,
            a.resolved_object.hex.as_str(),
            self.retention_handle,
        );
        let observed = (
            head.target.as_str(),
            head.oid.as_str(),
            head.retention_handle,
        );
        let original = self.evidence.reference_observation.is_none() && head.validate();
        original && scope == supplied && identity == observed
    }
    #[cfg(feature = "delivery_playbook")]
    fn synced_ref(&self, reference: &str) -> bool {
        let a = &self.admission;
        let e = &self.evidence;
        let scope = crate::main_sync::Scope::new(
            a.project_id,
            a.repository_id,
            a.repository_binding_generation,
            reference,
        );
        let proves = crate::main_sync::proves(
            e.main_sync.as_ref(),
            &scope,
            crate::main_sync::ObservedHead {
                head: &a.resolved_object.hex,
                handle: self.retention_handle,
                observed_at_ms: a.observed_at_ms,
            },
        );
        e.reference_observation.is_none() && e.publication_head.is_none() && proves
    }
}
pub fn artifact(bytes: &[u8]) -> Option<ArtifactRef> {
    let digest = Digest::of(bytes);
    Some(ArtifactRef {
        id: Text::new(format!(
            "aq.source-admission-observation/{}",
            digest.as_str()
        ))
        .ok()?,
        version: Text::new("1").ok()?,
        digest,
    })
}

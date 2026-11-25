//! Project a domain owner's outcome into the retained dashboard operation.
use super::*;

impl Disposition {
    pub fn unresolved(self) -> bool {
        matches!(self, Self::Pending | Self::Unknown)
    }
    fn recovery(self) -> Recovery {
        match self {
            Self::Pending | Self::Unknown => Recovery::LookupOriginal,
            Self::Conflict | Self::Rejected => Recovery::RefreshAndReview,
            Self::Confirmed => Recovery::None,
        }
    }
}
impl Outcome {
    fn valid_references(&self) -> bool {
        let bounded = self.receipt_refs.len() <= 100 && self.affected_entities.len() <= 100;
        let has_references = !self.receipt_refs.is_empty() || !self.affected_entities.is_empty();
        let confirmed = self.disposition == Disposition::Confirmed;
        bounded
            && (!has_references || confirmed)
            && self
                .receipt_refs
                .iter()
                .chain(&self.affected_entities)
                .all(EntityRef::valid)
    }
    fn valid_versions(&self) -> bool {
        if self.current_versions.is_empty() {
            return true;
        }
        self.disposition == Disposition::Conflict
            && self.current_versions.len() <= 32
            && self
                .current_versions
                .iter()
                .all(|(k, v)| safe(k, 128) && revision(v))
    }
    fn valid_activation(&self) -> bool {
        self.activation.is_none_or(|counts| {
            self.disposition == Disposition::Confirmed
                && counts.activated as usize == self.affected_entities.len()
        })
    }
    /// Return a changed operation, retaining the original evidence for malformed outcomes.
    /// The owner supplies the vocabulary check for a rejection reason.
    pub fn apply_to(
        self,
        original: &Operation,
        valid_reason: impl FnOnce(&str) -> bool,
    ) -> Option<Operation> {
        let reason_matches = self
            .reason
            .is_none_or(|reason| self.disposition == Disposition::Rejected && valid_reason(reason));
        let mut next = original.clone();
        let evidence_matches = self.valid_references() && self.valid_versions();
        let details_match = reason_matches && self.valid_activation();
        if evidence_matches && details_match {
            next.disposition = self.disposition;
            next.receipt_refs = self.receipt_refs;
            next.affected_entities = self.affected_entities;
            next.current_versions = self.current_versions;
            next.reason = self.reason.map(str::to_owned);
            next.activation = self.activation;
        } else {
            next.disposition = Disposition::Unknown;
        }
        if &next == original {
            return None;
        }
        next.recovery = next.disposition.recovery();
        Some(next)
    }
}
impl Operation {
    pub fn pending(request: &ActionRequest, scope: Scope, digest: String, stamp: String) -> Self {
        Self {
            operation_id: request.operation_id,
            disposition: Disposition::Pending,
            request_digest: digest,
            scope,
            created_at: stamp.clone(),
            updated_at: stamp,
            subject_refs: vec![request.subject.clone()],
            receipt_refs: vec![],
            affected_entities: vec![],
            recovery: Recovery::LookupOriginal,
            current_versions: Default::default(),
            reason: None,
            activation: None,
        }
    }
    /// Keep the original evidence and timestamp when a changed response exceeds its storage bound.
    pub fn bounded_update(mut self, original: &Self, timestamp: String) -> Self {
        self.updated_at = timestamp.max(original.updated_at.clone());
        if serde_json::to_vec(&self).is_ok_and(|bytes| bytes.len() <= 60_000) {
            return self;
        }
        let mut fallback = original.clone();
        fallback.disposition = Disposition::Unknown;
        fallback
    }
}

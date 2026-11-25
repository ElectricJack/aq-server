use super::*;

impl ConflictPath {
    fn valid_hex(&self) -> bool {
        let well_formed =
            self.encoding == "hex" && !self.value.is_empty() && self.value.len().is_multiple_of(2);
        well_formed
            && self
                .value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }
}
impl CandidateResult {
    fn matches_request(&self, context: &OperationContext, operation: &str, params: &Value) -> bool {
        let expected = if operation == "candidate.create" {
            context.operation_id.to_string()
        } else {
            params["candidate_id"].as_str().unwrap_or_default().into()
        };
        self.candidate_id.to_string() == expected
            && self.retention_handle == self.candidate_id
            && crate::refs::oid(&self.clean_prefix, false).is_ok()
    }
    fn valid_paths(&self) -> bool {
        let paths = &self.conflicting_paths;
        let counts = self.conflict_path_count >= paths.len()
            && (self.conflict_paths_truncated || self.conflict_path_count == paths.len());
        paths.len() <= 10_000
            && paths.iter().map(|p| p.value.len()).sum::<usize>() <= 1024 * 1024
            && paths.iter().all(ConflictPath::valid_hex)
            && counts
    }
    fn ready(&self) -> bool {
        let clean = self
            .candidate
            .as_ref()
            .is_some_and(|oid| oid.validate().is_ok() && *oid == self.clean_prefix);
        let no_conflict = self.conflict_step.is_none()
            && self.conflicting_paths.is_empty()
            && self.conflict_path_count == 0;
        clean && no_conflict && !self.conflict_paths_truncated
    }
    fn conflicted(&self, operation: &str, params: &Value) -> bool {
        self.candidate.is_none()
            && self.conflict_step.is_some_and(|step| {
                let supplied = operation != "candidate.create"
                    || params["sources"]
                        .as_array()
                        .is_some_and(|sources| step < sources.len());
                step < 64 && supplied
            })
    }
}
impl WireResult for CandidateResult {
    fn validate(&self, context: &OperationContext, operation: &str, params: &Value) -> bool {
        if !self.matches_request(context, operation, params) || !self.valid_paths() {
            return false;
        }
        match self.state {
            CandidateState::Ready => self.ready(),
            CandidateState::Conflicted => self.conflicted(operation, params),
        }
    }
}

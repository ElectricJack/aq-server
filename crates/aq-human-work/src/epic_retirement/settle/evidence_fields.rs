use super::*;
fn https(url: &str) -> bool {
    url.strip_prefix("https://").is_some_and(|rest| {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        !authority.is_empty()
            && !authority.contains('@')
            && url.bytes().all(|b| b.is_ascii_graphic())
    })
}

impl Evidence {
    pub fn valid(&self) -> bool {
        let fields = match self.kind {
            EvidenceKind::Merge => self.merge_fields(),
            EvidenceKind::Ordinary => self.ordinary_fields(),
            EvidenceKind::Integrated => self.integrated_fields(),
        };
        ids_valid([self.operation_id, self.project_id, self.registration_id])
            && self.approval_revision >= 1
            && self.evidence.valid()
            && fields
    }
    fn per_repository(&self) -> bool {
        self.repository
            .as_deref()
            .is_some_and(|s| (1..=96).contains(&s.len()))
            && self.sha.as_deref().is_some_and(commit)
            && matches!(
                self,
                Self {
                    revisions: None,
                    builds: None,
                    ..
                }
            )
    }
    fn merge_fields(&self) -> bool {
        self.per_repository()
            && self
                .pull_request
                .as_deref()
                .is_some_and(|u| u.len() <= 2048 && https(u))
            && self.supersedes_merge.is_none_or(|id| !id.is_nil())
            && matches!(
                self,
                Self {
                    passed: None,
                    flag_absent: None,
                    ..
                }
            )
    }
    fn ordinary_fields(&self) -> bool {
        self.per_repository()
            && matches!(
                self,
                Self {
                    passed: Some(_),
                    flag_absent: Some(_),
                    pull_request: None,
                    supersedes_merge: None,
                    ..
                }
            )
    }
    fn integrated_fields(&self) -> bool {
        matches!(
            self,
            Self {
                repository: None,
                sha: None,
                pull_request: None,
                supersedes_merge: None,
                flag_absent: None,
                passed: Some(_),
                ..
            }
        ) && self.valid_builds()
    }
    fn valid_builds(&self) -> bool {
        self.revisions
            .as_ref()
            .is_some_and(|r| (1..=32).contains(&r.len()) && commits(r))
            && self
                .builds
                .as_ref()
                .is_some_and(|b| (1..=32).contains(&b.len()) && b.values().all(Artifact::valid))
    }
}

use super::*;

/// Both the central git and the forge, scripted.
pub(super) struct Fake {
    pub(super) log: Mutex<Vec<String>>,
    pub(super) checks: Mutex<(ChecksState, Vec<String>)>,
    pub(super) pr_state: Mutex<PullRequestState>,
    pub(super) mergeability: Mutex<Mergeability>,
    pub(super) delete_unavailable: Mutex<bool>,
    pub(super) merge_sha: Mutex<String>,
}
#[async_trait::async_trait]
impl GitPort for Fake {
    async fn retain_ref(
        &self,
        _: Uuid,
        _: &GitTarget,
        reference: &str,
    ) -> std::result::Result<RetainedRef, GitFailure> {
        self.log.lock().unwrap().push(format!("retain {reference}"));
        Ok(RetainedRef {
            head: oid('9'),
            retention_handle: Uuid::new_v4(),
        })
    }
    async fn publish_ref(
        &self,
        _: Uuid,
        _: &GitTarget,
        reference: &str,
        expected_old: Option<&str>,
        _expected_source: Option<&str>,
    ) -> std::result::Result<RefPublication, GitFailure> {
        let lease = expected_old.unwrap_or("absent");
        self.log
            .lock()
            .unwrap()
            .push(format!("publish {reference} lease {lease}"));
        Ok(RefPublication::Published { head: oid('a') })
    }
    async fn delete_ref(
        &self,
        _: Uuid,
        _: &GitTarget,
        reference: &str,
        expected_head: Option<&str>,
    ) -> std::result::Result<RefDeletion, GitFailure> {
        assert_eq!(expected_head, Some(oid('a').as_str()));
        self.log.lock().unwrap().push(format!("delete {reference}"));
        if *self.delete_unavailable.lock().unwrap() {
            return Err(GitFailure {
                code: "transport".into(),
                message: "cleanup unavailable".into(),
                transient: true,
            });
        }
        Ok(RefDeletion::Deleted)
    }
}
fn pull(state: PullRequestState) -> PullRequest {
    PullRequest {
        number: 7,
        url: "https://example.test/pr/7".into(),
        head_sha: oid('a'),
        base_branch: "main".into(),
        base_sha: oid('b'),
        state,
        mergeability: Mergeability::Clean,
    }
}
#[async_trait::async_trait]
impl GitHubAdapter for Fake {
    async fn open_or_find(
        &self,
        _: &GitHubRepository,
        _: &str,
        _: &str,
        title: &str,
        _: &str,
    ) -> GitHubResult<PullRequest> {
        self.log.lock().unwrap().push(format!("open {title}"));
        Ok(pull(self.pr_state.lock().unwrap().clone()))
    }
    async fn pull_request(&self, _: &GitHubRepository, _: u64) -> GitHubResult<PullRequest> {
        Ok(PullRequest {
            mergeability: *self.mergeability.lock().unwrap(),
            ..pull(self.pr_state.lock().unwrap().clone())
        })
    }
    async fn checks(
        &self,
        repo: &GitHubRepository,
        pr: u64,
        _: Option<&[String]>,
    ) -> GitHubResult<Checks> {
        let (state, failed) = self.checks.lock().unwrap().clone();
        Ok(Checks {
            state,
            failed,
            required: vec![],
            pull_request: self.pull_request(repo, pr).await?,
        })
    }
    async fn merge(
        &self,
        _: &GitHubRepository,
        _: u64,
        method: MergeMethod,
        expected_head_sha: &str,
        _: &str,
    ) -> GitHubResult<MergeOutcome> {
        self.log
            .lock()
            .unwrap()
            .push(format!("merge {} {expected_head_sha}", method.as_str()));
        Ok(MergeOutcome::Merged {
            sha: self.merge_sha.lock().unwrap().clone(),
        })
    }
    async fn close(&self, _: &GitHubRepository, _: u64) -> GitHubResult<PullRequest> {
        self.log.lock().unwrap().push("close".into());
        Ok(pull(PullRequestState::Closed))
    }
    async fn delete_branch(
        &self,
        _: &GitHubRepository,
        _: &str,
    ) -> GitHubResult<aq_delivery::BranchDeletion> {
        Ok(aq_delivery::BranchDeletion::Deleted)
    }
}

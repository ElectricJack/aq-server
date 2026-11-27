use super::*;
// An empty set allows any producer. A nonempty set requires each listed App.
type RequiredChecks = BTreeMap<String, BTreeSet<u64>>;
type ReportedChecks = BTreeMap<String, BTreeMap<Option<u64>, ChecksState>>;

impl Session<'_> {
    /// Required check names and their App pins from rulesets and classic protection.
    pub(super) async fn required_checks(&self, base: &str) -> GitHubResult<RequiredChecks> {
        Ok(self.base_rules(base).await?.0)
    }
    /// [`Session::required_checks`], and whether a rule requires a merge queue.
    pub(super) async fn base_rules(&self, base: &str) -> GitHubResult<(RequiredChecks, bool)> {
        let mut required = RequiredChecks::new();
        let rules = self
            .pages(&format!("/rules/branches/{base}"), |v| array(v, None))
            .await?;
        apply_rules(&mut required, &rules);
        let merge_queue = rules.iter().any(|r| r["type"] == "merge_queue");
        let branch: Value = self
            .call(ApiRequest::new(Method::GET, &format!("/branches/{base}")))
            .await?;
        apply_branch_protection(&mut required, &branch);
        Ok((required, merge_queue))
    }

    /// The ids of the failed check runs on `sha` named in `names`, by name. A
    /// GitHub Actions check run's id is its job's id.
    pub(super) async fn failed_runs(
        &self,
        sha: &str,
        names: &[String],
    ) -> GitHubResult<BTreeMap<String, u64>> {
        let runs = self
            .pages(&format!("/commits/{sha}/check-runs"), |v| {
                array(v, Some("check_runs"))
            })
            .await?;
        let mut failed = BTreeMap::new();
        for run in runs {
            let broken = run["status"] == "completed"
                && !matches!(
                    run["conclusion"].as_str(),
                    Some("success" | "neutral" | "skipped") | None
                );
            if let (true, Some(name), Some(id)) = (broken, run["name"].as_str(), run["id"].as_u64())
                && names.iter().any(|n| n == name)
            {
                failed.entry(name.to_string()).or_insert(id);
            }
        }
        Ok(failed)
    }

    /// Check runs and commit statuses on `sha`, grouped by name and App.
    pub(super) async fn reported_checks(&self, sha: &str) -> GitHubResult<ReportedChecks> {
        let mut reported = ReportedChecks::new();
        let runs = self
            .pages(&format!("/commits/{sha}/check-runs"), |v| {
                array(v, Some("check_runs"))
            })
            .await?;
        for run in runs {
            let Some(name) = run["name"].as_str() else {
                return Err(GitHubApiError::Protocol("check run without a name".into()));
            };
            let state = match (run["status"].as_str(), run["conclusion"].as_str()) {
                (Some("completed"), Some("success" | "neutral" | "skipped")) => ChecksState::Passed,
                (Some("completed"), Some(_)) => ChecksState::Failed,
                _ => ChecksState::Pending,
            };
            record(&mut reported, name, run["app"]["id"].as_u64(), state);
        }
        let statuses = self
            .pages(&format!("/commits/{sha}/status"), |v| {
                array(v, Some("statuses"))
            })
            .await?;
        for status in statuses {
            let Some(name) = status["context"].as_str() else {
                return Err(GitHubApiError::Protocol(
                    "commit status without a context".into(),
                ));
            };
            let state = match status["state"].as_str() {
                Some("success") => ChecksState::Passed,
                Some("failure" | "error") => ChecksState::Failed,
                _ => ChecksState::Pending,
            };
            // The combined status response supplies no GitHub App identity.
            record(&mut reported, name, None, state);
        }
        Ok(reported)
    }
}

/// The array at `key` (or the body itself).
fn array(v: Value, key: Option<&str>) -> GitHubResult<Vec<Value>> {
    let v = match key {
        Some(key) => v.get(key).cloned().unwrap_or(Value::Null),
        None => v,
    };
    match v {
        Value::Array(items) => Ok(items),
        _ => Err(GitHubApiError::Protocol("expected a list".into())),
    }
}
fn require_check(required: &mut RequiredChecks, check: &Value, app_field: &str) {
    if let Some(name) = check["context"].as_str() {
        let app_id = check[app_field].as_u64().filter(|id| *id > 0);
        required.entry(name.to_string()).or_default().extend(app_id);
    }
}

fn severity(state: ChecksState) -> u8 {
    match state {
        ChecksState::Passed => 0,
        ChecksState::Pending => 1,
        ChecksState::Failed => 2,
    }
}

/// Repeated reports from the same producer keep the worse state.
fn record(reported: &mut ReportedChecks, name: &str, app_id: Option<u64>, state: ChecksState) {
    reported
        .entry(name.to_string())
        .or_default()
        .entry(app_id)
        .and_modify(|current| {
            if severity(state) > severity(*current) {
                *current = state;
            }
        })
        .or_insert(state);
}

fn worst_state(states: impl Iterator<Item = ChecksState>) -> ChecksState {
    states
        .max_by_key(|state| severity(*state))
        .unwrap_or(ChecksState::Pending)
}

/// Missing reports from a required App stay pending.
pub(super) fn evaluate(
    required: &RequiredChecks,
    reported: &ReportedChecks,
) -> (ChecksState, Vec<String>) {
    let counted: Vec<(&str, ChecksState)> = if required.is_empty() {
        reported
            .iter()
            .map(|(name, states)| (name.as_str(), worst_state(states.values().copied())))
            .collect()
    } else {
        required
            .iter()
            .map(|(name, apps)| {
                let state = match reported.get(name) {
                    None => ChecksState::Pending,
                    Some(states) if apps.is_empty() => worst_state(states.values().copied()),
                    Some(states) => worst_state(apps.iter().map(|id| {
                        states
                            .get(&Some(*id))
                            .copied()
                            .unwrap_or(ChecksState::Pending)
                    })),
                };
                (name.as_str(), state)
            })
            .collect()
    };
    let failed: BTreeSet<String> = counted
        .iter()
        .filter(|(_, s)| *s == ChecksState::Failed)
        .map(|(n, _)| n.to_string())
        .collect();
    let state = if !failed.is_empty() {
        ChecksState::Failed
    } else if counted.is_empty() || counted.iter().any(|(_, s)| *s == ChecksState::Pending) {
        ChecksState::Pending
    } else {
        ChecksState::Passed
    };
    (state, failed.into_iter().collect())
}

fn apply_rules(required: &mut RequiredChecks, rules: &[Value]) {
    for rule in rules
        .iter()
        .filter(|r| r["type"] == "required_status_checks")
    {
        for check in rule["parameters"]["required_status_checks"]
            .as_array()
            .into_iter()
            .flatten()
        {
            require_check(required, check, "integration_id");
        }
    }
}

fn apply_branch_protection(required: &mut RequiredChecks, branch: &Value) {
    if branch["protected"] == true {
        let checks = &branch["protection"]["required_status_checks"];
        // Legacy contexts can repeat names from the structured checks.
        for context in checks["contexts"].as_array().into_iter().flatten() {
            if let Some(name) = context.as_str() {
                required.entry(name.to_string()).or_default();
            }
        }
        for check in checks["checks"].as_array().into_iter().flatten() {
            require_check(required, check, "app_id");
        }
    }
}

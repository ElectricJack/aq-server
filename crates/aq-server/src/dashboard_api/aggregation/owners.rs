use super::*;

impl OverviewRead<'_> {
    pub(super) async fn reviews(&self) -> Result<Contribution> {
        let (_, runtime, user) = self.request.parts();
        let projects = &self.projects;
        let truncated = self.truncated;
        let owner = runtime.reviews.as_ref().ok_or(Error::Unavailable)?;
        let mut c = Contribution::default();
        let mut n = 0;
        let mut more = truncated;
        for p in projects {
            let page = owner
                .inbox_lean(
                    user.id,
                    &crate::reviews::InboxQuery {
                        project_id: Some(p.label.id),
                        state: crate::reviews::InboxState::Open,
                        order: crate::reviews::InboxOrder::Decision,
                        limit: 25,
                        cursor: None,
                    },
                )
                .await?;
            let page_partial = page.next_cursor.is_some() || page.sampled;
            more |= page_partial;
            c.sources.push(source(
                "reviews",
                Some(p.label.clone()),
                SourceSample {
                    at: page.server_time_ms,
                    count: page.items.len(),
                    truncated: page_partial || truncated,
                },
                Some("sampled_code_review_decisions; decision_transition_time_unavailable"),
            )?);
            for item in page.items {
                n += add_decision(&mut c, &p.label, item)?;
            }
        }
        self.empty_source(&mut c, "reviews", None)?;
        // This is the observed eligible-decision count, never a claim about questions.
        c.needs_you = Some(total(n, more));
        Ok(c)
    }

    pub(super) async fn results(&self, mounted: bool) -> Result<Contribution> {
        let (state, _, user) = self.request.parts();
        let projects = &self.projects;
        let truncated = self.truncated;
        if !mounted {
            return Err(Error::Unavailable);
        }
        let mut c = Contribution::default();
        for p in projects {
            let page = Box::pin(human_results::read_as(
                state,
                user,
                &human_results::Page {
                    profile: Some(human_results::ReadProfile::RecordedNativeV2),
                    project_id: Some(p.label.id),
                    after: None,
                    limit: Some(25),
                },
                None,
            ))
            .await?;
            c.sources.push(source(
                "results",
                Some(p.label.clone()),
                SourceSample {
                    at: page.server_time_ms,
                    count: page.items.len(),
                    truncated: page.next_after.is_some() || truncated,
                },
                Some("recorded_native_results_only"),
            )?);
            for r in page.items {
                add_result(&mut c, &p.label, r)?;
            }
        }
        self.empty_source(&mut c, "results", Some("recorded_native_results_only"))?;
        Ok(c)
    }

    pub(super) async fn settlements(&self) -> Result<Contribution> {
        let (state, runtime, user) = self.request.parts();
        let projects = &self.projects;
        let truncated = self.truncated;
        let mut c = Contribution::default();
        let Some(reviews) = &runtime.reviews else {
            c.sources
                .push(unavailable("settlement", "review_owner_not_mounted"));
            return Ok(c);
        };
        let owner = settlement::Service::new(state.clone(), reviews.as_ref().clone());
        let mut transaction = tx(runtime).await?;
        let ids: Vec<String> = projects.iter().map(|p| p.label.id.to_string()).collect();
        let rows=sqlx::query("SELECT s.operation_id,s.receipt_digest,l.execution_spec#>>'{lifecycle,project_id}' AS project FROM profiled_settlement_evaluations s JOIN profiled_launches l USING(attempt_id) WHERE s.actor_id=$1 AND l.execution_spec#>>'{lifecycle,project_id}'=ANY($2) ORDER BY s.created_at_ms DESC,s.operation_id LIMIT 26").bind(user.id).bind(ids).fetch_all(&mut *transaction).await?;
        transaction.commit().await?;
        for row in rows.iter().take(25) {
            let receipt = owner.lookup(user, row.get("operation_id")).await?;
            let project = projects
                .iter()
                .find(|p| p.label.id.to_string() == row.get::<String, _>("project"))
                .ok_or(Error::Unavailable)?;
            // Owner lookup verifies this immutable evaluation's retained receipt digest.
            // Acceptance does not establish Graph task completion.
            let outcome = match receipt.disposition {
                settlement::EvaluationDisposition::Accepted => "settlement_accepted",
                settlement::EvaluationDisposition::Rejected => "settlement_rejected",
                settlement::EvaluationDisposition::Stale => "settlement_stale",
                settlement::EvaluationDisposition::NeedsReconciliation => {
                    "settlement_needs_reconciliation"
                }
            };
            c.outcomes.push(RecentOutcome {
                subject: TitledRef {
                    kind: "settlement_evaluation".into(),
                    id: receipt.operation_id,
                    revision: Some(digest(&row.get::<String, _>("receipt_digest"))?),
                    title: "Settlement evaluation".into(),
                },
                project: project.label.clone(),
                outcome: outcome.into(),
                at: timestamp(receipt.recorded_at_ms)?,
            });
        }
        c.sources.push(source(
            "settlement",
            None,
            SourceSample {
                at: stamp(state)?,
                count: rows.len().min(25),
                truncated: truncated || rows.len() > 25,
            },
            None,
        )?);
        Ok(c)
    }

    pub(super) async fn operations(&self) -> Result<Contribution> {
        let (state, runtime, user) = self.request.parts();
        let scope = self.scope;
        let labels = &self.labels;
        let truncated = self.truncated;
        let mut transaction = tx(runtime).await?;
        let rows: Vec<Uuid> = sqlx::query_scalar("SELECT operation_id FROM dashboard_operations WHERE principal_id=$1 AND retired_at IS NULL AND disposition IN ('pending','unknown') AND ($2::uuid IS NULL OR scope->>'project_id'=$2::text) ORDER BY created_at,operation_id LIMIT 101").bind(user.id).bind(scope.project()).fetch_all(&mut *transaction).await?;
        transaction.commit().await?;
        let mut c = Contribution::default();
        for id in rows.iter().take(MEMBERS) {
            let op = actions::journal::lookup(&runtime.pool, user.id, *id)
                .await
                .map_err(|_| Error::Unavailable)?;
            if let Some(item) = pending_operation(op, labels)? {
                c.attention.push(item);
            }
        }
        c.sources.push(source(
            "operations",
            None,
            SourceSample {
                at: stamp(state)?,
                count: c.attention.len(),
                truncated: truncated || rows.len() > MEMBERS,
            },
            Some("dashboard_journal_only"),
        )?);
        Ok(c)
    }
}

impl OverviewRead<'_> {
    /// Spec 0012 §8 step 3 surfaces each internal run a supervisor could not fix.
    /// The link opens the affected item when the dashboard supports it. Otherwise
    /// it opens the run and the impact description identifies the affected item.
    pub(super) async fn internal_work(&self) -> Result<Contribution> {
        let (state, runtime, _) = self.request.parts();
        let scope = self.scope;
        let labels = &self.labels;

        let mut transaction = tx(runtime).await?;
        let rows = sqlx::query("SELECT id,xmin::text AS revision,affected_kind,affected_id,project_id,diagnosis,escalated_at_ms,CASE WHEN purpose='routing_classification' AND affected_kind='task' AND request->>'input' ~ '^(not_offered|unroutable|unrouted)@' THEN left(detail,600) END AS unrouted FROM internal_tasks WHERE escalation='owner' AND ($1::uuid IS NULL OR project_id=$1) ORDER BY escalated_at_ms DESC,id LIMIT 101")
        .bind(scope.project()).fetch_all(&mut *transaction).await?;
        transaction.commit().await?;
        let mut c = Contribution::default();
        for row in rows.iter().take(MEMBERS) {
            c.attention.push(internal_attention(row, labels)?);
        }
        let (n, more) = (c.attention.len(), rows.len() > MEMBERS);
        c.sources.push(source(
            "internal_work",
            None,
            SourceSample {
                at: stamp(state)?,
                count: n,
                truncated: more,
            },
            None,
        )?);
        Ok(c)
    }
}

fn add_decision(
    c: &mut Contribution,
    project: &ProjectRef,
    item: aq_dashboard_contract::review_view::InboxItem,
) -> Result<usize> {
    require_project(project, item.project_id)?;
    c.attention.push(attention(
        AttentionKind::Decision,
        reference(
            "decision",
            item.decision_id,
            Revision(item.decision_revision.to_string()).into(),
        ),
        Some(project.clone()),
        AttentionDescription {
            title: "Waiting decision",
            reason: if item.eligible_owner {
                "An eligible owner can review this pending decision"
            } else {
                "Your requested decision is pending"
            },
            since: None,
        },
    ));
    Ok(usize::from(item.eligible_owner))
}

fn add_result(
    c: &mut Contribution,
    project: &ProjectRef,
    r: aq_dashboard_contract::result_metadata::ResultMetadata,
) -> Result<()> {
    require_project(project, r.project_id)?;
    let revision = digest(&r.revision)?;
    let at = timestamp(r.recorded_at_ms)?;
    c.outcomes.push(RecentOutcome {
        subject: TitledRef {
            kind: "result".into(),
            id: r.report_id,
            revision: Some(revision.clone()),
            title: "Recorded process result".into(),
        },
        project: project.clone(),
        outcome: format!("process_{}", r.process_outcome),
        at: at.clone(),
    });
    if r.current_report_head && r.process_outcome == "failed" {
        let detail = AttentionDescription {
            title: "Failed process result",
            reason: "The current retained result reports process failure; task settlement is separate",
            since: Some(at),
        };
        c.attention.push(attention(
            AttentionKind::Failed,
            reference("result", r.report_id, revision),
            Some(project.clone()),
            detail,
        ));
    }
    Ok(())
}

fn internal_attention(
    row: &sqlx::postgres::PgRow,
    labels: &BTreeMap<Uuid, ProjectRef>,
) -> Result<AttentionItem> {
    const OPENABLE: [&str; 5] = ["task", "decision", "session", "knowledge", "playbook_run"];
    let (run, kind, id): (Uuid, String, String) = (
        row.get("id"),
        row.get("affected_kind"),
        row.get("affected_id"),
    );
    let project = row
        .get::<Option<Uuid>, _>("project_id")
        .and_then(|id| labels.get(&id).cloned());
    let affected = id
        .parse::<Uuid>()
        .ok()
        .filter(|_| OPENABLE.contains(&kind.as_str()));
    let subject = EntityRef {
        kind: if affected.is_some() {
            kind.clone()
        } else {
            "internal_run".into()
        },
        id: affected.unwrap_or(run),
        revision: Some(Revision(row.get("revision")).into()),
    };
    let diagnosis: Option<String> = row.get("diagnosis");
    let since = row.get::<Option<i64>, _>("escalated_at_ms");
    let since = since.map(|ms| timestamp(ms as u64)).transpose()?;
    // Spec 0011 §4 W6: a task routing could not route says why, in words.
    let unrouted: Option<String> = row.get("unrouted");
    let reason =
        (unrouted.as_deref().or(diagnosis.as_deref())).unwrap_or("A supervisor could not fix it");
    let title = match unrouted {
        Some(_) => "A task could not be routed",
        None => "Internal work failed",
    };
    let mut item = attention(
        AttentionKind::InternalWork,
        subject,
        project,
        AttentionDescription {
            title,
            reason,
            since,
        },
    );
    item.id = format!("internal_work:{run}");
    if affected.is_none() {
        item.impact = aq_internal_work::Affected::describe(&kind, &id);
    }
    Ok(item)
}

fn pending_operation(
    op: actions::Operation,
    labels: &BTreeMap<Uuid, ProjectRef>,
) -> Result<Option<AttentionItem>> {
    if !matches!(
        op.disposition,
        actions::Disposition::Pending | actions::Disposition::Unknown
    ) {
        return Ok(None);
    }
    let project = op.scope.project().and_then(|id| labels.get(&id).cloned());
    if op.scope.project().is_some() && project.is_none() {
        return Ok(None);
    }
    Ok(Some(attention(
        AttentionKind::UncertainOperation,
        reference(
            "operation",
            op.operation_id,
            digest(
                op.request_digest
                    .strip_prefix("sha256:")
                    .ok_or(Error::Unavailable)?,
            )?,
        ),
        project,
        AttentionDescription {
            title: "Operation awaiting confirmation",
            reason: "Look up the original operation before attempting another action",
            since: Some(Timestamp(op.created_at)),
        },
    )))
}

fn require_project(project: &ProjectRef, observed: Uuid) -> Result<()> {
    if project.id != observed {
        return Err(Error::Unavailable);
    }
    Ok(())
}

use super::*;

pub(super) struct EvaluatedRow {
    pub(super) trace: CandidateTrace,
    pub(super) choice: Option<ConcreteChoice>,
    pub(super) revision: u64,
}

pub(super) fn rank(mut rows: Vec<EvaluatedRow>, route: Option<&RouteLimits>) -> CoreOutcome {
    if let Some(route) = route {
        rows.sort_by_key(|row| {
            crate::route::preference(route, &row.trace.provider, &row.trace.harness)
        });
    }
    // Each row has already passed or failed its own eligibility checks. A
    // refusal on one candidate has no authority over another candidate.
    let selected = rows.iter_mut().enumerate().find_map(|(index, row)| {
        row.choice
            .take()
            .map(|choice| (choice, row.revision, index))
    });
    let reason_codes = if selected.is_none() {
        vec!["no_eligible_candidate".into()]
    } else {
        Vec::new()
    };
    CoreOutcome {
        reason_codes,
        selected,
        traces: rows.into_iter().map(|r| r.trace).collect(),
    }
}

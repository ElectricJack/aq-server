//! Bounded public inspection projections. Full snapshots remain a privileged API.
use crate::{Intent, Receipt, Run, State, StorageError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectionKind {
    Runs,
    Effects,
    Receipts,
}
impl InspectionKind {
    pub fn key(self) -> &'static str {
        match self {
            Self::Runs => "runs",
            Self::Effects => "effects",
            Self::Receipts => "receipts",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectionQuery {
    pub kind: InspectionKind,
    pub run: Option<String>,
    pub after: Option<String>,
    pub limit: usize,
}
impl InspectionQuery {
    pub fn validate(&self) -> Result<(), StorageError> {
        if self.limit == 0
            || self.limit > 1000
            || self
                .run
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 256)
            || self.after.as_ref().is_some_and(|s| s.len() > 256)
        {
            return Err(StorageError::Backend(
                "invalid inspection page bounds".into(),
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectionPage {
    pub items: Vec<Value>,
    pub next: Option<String>,
    pub revision: u64,
}
#[derive(Debug, Clone)]
pub struct InspectionRow {
    pub kind: InspectionKind,
    pub key: String,
    pub run: String,
    pub sequence: u64,
    pub value: Value,
}
pub fn public_run(r: &Run) -> Value {
    json!({"id":r.id,"artifact":r.artifact,"entry":r.entry,"step":r.step,"status":r.status,"outcome":r.outcome,"reason":r.reason.as_ref().map(|_|"Progress requires attention; see transition receipts and host recovery status"),"root":r.root,"parent":r.parent,"origin":r.origin,"children":r.children,"dependencies":r.dependencies,"pending":r.pending,"visits":r.visits,"admitted_at_ms":r.admitted_at_ms})
}
pub fn public_intent(i: &Intent) -> Value {
    json!({"id":i.id,"run":i.run,"step":i.step,"command":i.command.name,"fingerprint":i.command.fingerprint().ok(),"status":i.status,"handle":i.handle,"reason":i.reason.as_ref().map(|_|"Host detail omitted from public inspection"),"recovery":i.command.recovery,"deadline_at_ms":i.deadline_at_ms,"cancel_requested":i.cancel_requested,"cancel_dispatched":i.cancel_dispatched})
}
pub fn public_receipt(r: &Receipt) -> Value {
    // Unknown receipt kinds and fields are private by default. The only value
    // projection is command_result.public, already allowlisted by its contract.
    let allowed: &[&str] = match r.kind.as_str() {
        "admitted" => &["artifact"],
        "branch" => &["outcome", "next"],
        "terminal" => &["status", "outcome"],
        "intent" => &["operation"],
        "iteration" => &["path"],
        "business_retry" => &["attempt"],
        "child_admitted" => &["child", "mode"],
        "command_result" => &["operation", "outcome", "public"],
        "cancel_response" => &["operation", "disposition"],
        "destination_blocked" => &["destination", "artifact"],
        "dispatch"
        | "cancel_dispatch"
        | "operation_accepted"
        | "operation_unknown"
        | "operation_rejected"
        | "invalid_output"
        | "late_result"
        | "command_deadline_expired" => &["operation"],
        "recovery_requested" => &["operation", "request"],
        "garbage_collected" => &["runs", "events", "artifacts", "receipts"],
        _ => &[],
    };
    let detail = Value::Object(
        r.detail
            .as_object()
            .into_iter()
            .flat_map(|o| o.iter())
            .filter(|(k, _)| allowed.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    );
    json!({"sequence":r.sequence,"run":r.run,"step":r.step,"at_ms":r.at_ms,"kind":r.kind,"detail":detail})
}

pub fn rows(state: &State) -> Vec<InspectionRow> {
    let mut rows =
        Vec::with_capacity(state.runs.len() + state.intents.len() + state.receipts.len());
    rows.extend(state.runs.values().map(|r| InspectionRow {
        kind: InspectionKind::Runs,
        key: r.id.clone(),
        run: r.id.clone(),
        sequence: 0,
        value: public_run(r),
    }));
    rows.extend(state.intents.values().map(|i| InspectionRow {
        kind: InspectionKind::Effects,
        key: i.id.clone(),
        run: i.run.clone(),
        sequence: i.invocation,
        value: public_intent(i),
    }));
    rows.extend(state.receipts.iter().map(|r| InspectionRow {
        kind: InspectionKind::Receipts,
        key: format!("{:020}", r.sequence),
        run: r.run.clone(),
        sequence: r.sequence,
        value: public_receipt(r),
    }));
    rows
}
pub fn page(
    state: &State,
    revision: u64,
    query: &InspectionQuery,
) -> Result<InspectionPage, StorageError> {
    query.validate()?;
    let mut selected = rows(state)
        .into_iter()
        .filter(|row| {
            row.kind == query.kind
                && query.run.as_ref().is_none_or(|id| &row.run == id)
                && query.after.as_ref().is_none_or(|cursor| &row.key > cursor)
        })
        .collect::<Vec<_>>();
    selected.sort_by(|a, b| a.key.cmp(&b.key));
    let has_more = selected.len() > query.limit;
    selected.truncate(query.limit);
    let next = has_more.then(|| selected.last().unwrap().key.clone());
    Ok(InspectionPage {
        items: selected.into_iter().map(|row| row.value).collect(),
        next,
        revision,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipt_pages_are_stable_and_hide_private_diagnostics() {
        let mut state = State::default();
        for sequence in [10, 2, 30] {
            state.receipts.push(Receipt{sequence,run:"r".into(),step:"s".into(),at_ms:1,kind:"recovery_requested".into(),detail:json!({"reason":"secret-value","operation":"op","evidence_refs":["private-evidence"],"secret":"unclassified-secret"})});
        }
        let mut q = InspectionQuery {
            kind: InspectionKind::Receipts,
            run: Some("r".into()),
            after: None,
            limit: 2,
        };
        let first = page(&state, 5, &q).unwrap();
        assert_eq!(first.items[0]["sequence"], 2);
        assert_eq!(first.items[1]["sequence"], 10);
        assert!(
            !serde_json::to_string(&first)
                .unwrap()
                .contains("secret-value")
        );
        assert!(
            !serde_json::to_string(&first)
                .unwrap()
                .contains("private-evidence")
        );
        assert!(
            !serde_json::to_string(&first)
                .unwrap()
                .contains("unclassified-secret")
        );
        q.after = first.next;
        let second = page(&state, 5, &q).unwrap();
        assert_eq!(second.items[0]["sequence"], 30);
        assert!(second.next.is_none());
        let intent = Receipt {
            sequence: 31,
            run: "r".into(),
            step: "s".into(),
            at_ms: 1,
            kind: "intent".into(),
            detail: json!({"operation":"op","input_digest":"guessable-secret-hash"}),
        };
        assert_eq!(public_receipt(&intent)["detail"], json!({"operation":"op"}));
    }
    #[test]
    fn unbounded_or_invalid_pages_refused() {
        let q = InspectionQuery {
            kind: InspectionKind::Runs,
            run: None,
            after: None,
            limit: 0,
        };
        assert!(q.validate().is_err());
    }
}

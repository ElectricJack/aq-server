use super::*;
use sqlx::postgres::PgRow;

/// The subject coordinates and scope bound to this wait member's owner read.
pub(super) struct Decision<'a> {
    pub scope: &'a wire::Scope,
    pub id: Uuid,
    pub subject: Uuid,
    pub revision: &'a wire::Text,
    pub question: &'a wire::Digest,
}

impl Decision<'_> {
    pub(super) async fn read(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        now: u64,
    ) -> Result<Option<Observed>> {
        let row = sqlx::query("SELECT d.subject_digest,d.request_digest,d.decision_revision,d.state,d.expires_at_ms,s.report_id,s.project_id,s.workspace_id,f.receipt_digest FROM human_decision_requests d JOIN review_subjects s USING(subject_digest) LEFT JOIN human_decision_finality f USING(decision_id) WHERE d.decision_id=$1")
        .bind(self.id)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(row) = row {
            return self.observe(&row, Owner::Review, now);
        }
        let row = sqlx::query("SELECT task_id,project_id,workspace_id,subject_revision,question_digest,decision_revision,state,expires_at_ms,receipt_digest FROM question_decisions WHERE decision_id=$1")
        .bind(self.id)
        .fetch_optional(&mut **tx)
        .await?;
        match row {
            Some(row) => self.observe(&row, Owner::Question, now),
            None => Ok(None),
        }
    }

    fn observe(&self, row: &PgRow, owner: Owner, now: u64) -> Result<Option<Observed>> {
        let (id, revision, question) = owner.columns();
        let subject = (
            row.get::<Uuid, _>(id),
            row.get::<&str, _>(revision),
            row.get::<&str, _>(question),
        );
        let scope = (
            row.get::<Uuid, _>("project_id"),
            row.get::<Uuid, _>("workspace_id"),
        );
        if subject != (self.subject, self.revision.as_str(), self.question.as_str())
            || scope != (self.scope.project_id, self.scope.workspace_id)
        {
            return Ok(None);
        }
        project(row, owner.prefix(), self.id, now)
    }
}

#[derive(Clone, Copy)]
enum Owner {
    Review,
    Question,
}
impl Owner {
    fn columns(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Review => ("report_id", "subject_digest", "request_digest"),
            Self::Question => ("task_id", "subject_revision", "question_digest"),
        }
    }
    fn prefix(self) -> &'static str {
        match self {
            Self::Review => "human",
            Self::Question => "question",
        }
    }
}

fn decision_receipt(
    kind: &str,
    decision_id: Uuid,
    state: &str,
    receipt: Option<&str>,
) -> Result<wire::ArtifactRef> {
    let digest = match receipt {
        Some(digest) => serde_json::from_value(json!(digest))?,
        None => value_digest(&json!({"decision_id":decision_id,"state":state}))?,
    };
    Ok(artifact(
        format!("{kind}-decision-receipt-{decision_id}"),
        digest,
    ))
}

fn project(row: &PgRow, kind: &str, decision_id: Uuid, now: u64) -> Result<Option<Observed>> {
    let revision: i64 = row.get("decision_revision");
    let decision_state: String = row.get("state");
    let receipt: Option<String> = row.get("receipt_digest");
    let expired = row.get::<i64, _>("expires_at_ms") as u64 <= now;
    let state = match (decision_state.as_str(), &receipt) {
        ("answered", Some(_)) => wire::MemberState::Satisfied,
        ("answered", None) => return Ok(None),
        ("pending", _) if !expired => wire::MemberState::Pending,
        _ => wire::MemberState::SourceFailed,
    };
    let result = match state {
        wire::MemberState::Pending => None,
        _ => Some(decision_receipt(
            kind,
            decision_id,
            &decision_state,
            receipt.as_deref(),
        )?),
    };
    Ok(Some(Observed {
        snapshot: snapshot(
            format!("{kind}-decision-{decision_id}"),
            &json!({"decision_id":decision_id,"revision":revision,"state":decision_state,"receipt":receipt}),
        )?,
        revision: revision.to_string(),
        state,
        result,
    }))
}

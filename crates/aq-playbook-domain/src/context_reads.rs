//! Project-scoped context observations returned to a host command.
use crate::{
    Result,
    host_values::{HostResult, subject, text},
};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

pub struct ContextQuery<'a> {
    pub subject: &'a str,
    snapshot: &'a str,
}
impl<'a> ContextQuery<'a> {
    pub fn parse(input: &'a Value) -> Result<Self> {
        let subject = text(&input["subject"], "id")?;
        let snapshot = text(input, "snapshot")?;
        Ok(Self { subject, snapshot })
    }
    pub async fn read(
        &self,
        db: &mut PgConnection,
        project: Uuid,
        kind: &str,
    ) -> Result<Observation> {
        match kind {
            "project" => project_binding(db, project).await,
            "execution_options" => self.execution_options(db, project).await,
            "evidence" => self.evidence(db, project).await,
            // Historical profile 5 keeps its original incomplete health view.
            _ => Ok(Observation::incomplete(self.subject)),
        }
    }
    async fn execution_options(&self, db: &mut PgConnection, project: Uuid) -> Result<Observation> {
        let Ok(attempt) = Uuid::parse_str(self.subject) else {
            return Ok(Observation::incomplete(self.subject));
        };
        let selected:Option<(Uuid,String)>=sqlx::query_as("SELECT s.attempt_id,s.selection_digest FROM intelligence_attempt_selections s JOIN profiled_launches l USING(attempt_id) JOIN admission_source_observations o ON o.observation_id=l.source_observation_id WHERE s.attempt_id=$1 AND o.project_id=$2").bind(attempt).bind(project).fetch_optional(db).await?;
        Ok(match selected {
            Some((id, snapshot)) => {
                Observation::observed(format!("aq.execution.attempt/{id}"), 0, snapshot)
            }
            None => Observation::incomplete(self.subject),
        })
    }
    async fn evidence(&self, db: &mut PgConnection, project: Uuid) -> Result<Observation> {
        let result:Option<(Uuid,String)>=sqlx::query_as("SELECT result_id,report_revision FROM result_revision_pins WHERE project_id=$1 AND report_id::text=$2").bind(project).bind(self.subject).fetch_optional(db).await?;
        Ok(match result {
            Some((id, revision)) => Observation::observed(format!("aq.result/{id}"), 0, revision),
            None => Observation::incomplete(self.subject),
        })
    }
}
async fn project_binding(db: &mut PgConnection, project: Uuid) -> Result<Observation> {
    let binding: Option<(i64, String)> = sqlx::query_as(
        "SELECT revision,md5(binding::text) FROM project_execution_bindings WHERE project_id=$1",
    )
    .bind(project)
    .fetch_optional(db)
    .await?;
    let reference = format!("aq.project/{project}");
    Ok(match binding {
        Some((revision, snapshot)) => Observation::observed(reference, revision, snapshot),
        None => Observation::incomplete(reference),
    })
}

pub struct Observation {
    outcome: &'static str,
    reference: String,
    revision: i64,
    snapshot: String,
}
impl Observation {
    pub fn observed(reference: String, revision: i64, snapshot: String) -> Self {
        Self {
            outcome: "observed",
            reference,
            revision,
            snapshot,
        }
    }
    pub fn refused(reference: impl Into<String>) -> Self {
        Self::missing("refused", reference)
    }
    pub fn incomplete(reference: impl Into<String>) -> Self {
        Self::missing("incomplete", reference)
    }
    fn missing(outcome: &'static str, reference: impl Into<String>) -> Self {
        Self {
            outcome,
            reference: reference.into(),
            revision: 0,
            snapshot: String::new(),
        }
    }
    pub fn result(self, query: &ContextQuery<'_>, now: i64) -> HostResult {
        let stale = !query.snapshot.is_empty() && query.snapshot != self.snapshot;
        let outcome = if self.outcome == "observed" && stale {
            "expired"
        } else {
            self.outcome
        };
        HostResult {
            outcome: outcome.into(),
            output: json!({
                "complete":outcome=="observed", "expires_at_ms":now+60_000, "next_page":null,"observed_at_ms":now,"reference":self.reference,"snapshot":self.snapshot,"subject":subject(query.subject,self.revision),"visibility":"project",
            }),
        }
    }
}

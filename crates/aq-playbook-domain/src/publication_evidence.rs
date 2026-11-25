//! Verify a candidate’s published evidence requirements against issuer-owned records.
use crate::{
    Error, Namespace, Result,
    host_requests::CHECK_EXECUTION_PROFILE,
    host_values::{WORK_REF, handle, same_subject},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row, postgres::PgRow};
use std::collections::BTreeSet;
use uuid::Uuid;
const CHECK_EVIDENCE: &str = "check-receipt:";
const JUDGMENT_EVIDENCE: &str = "judgment:";
const REVIEW_EVIDENCE: &str = "aq.review.decision/";
const MAX_EVIDENCE: usize = 64;
/// Evidence a project's integration policy requires before publication.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidencePolicy {
    /// Check ids that each need a passing D6 check receipt.
    pub checks: Vec<String>,
    /// An approving review decision from an authorized issuer.
    pub review: bool,
    /// A D6 judgment assessed with one of these choices; empty = none needed.
    pub judgment_choices: Vec<String>,
}
impl EvidencePolicy {
    pub fn names_nothing(&self) -> bool {
        self.checks.is_empty() && !self.review && self.judgment_choices.is_empty()
    }
}

/// A review decision as its owning service reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewEvidence {
    pub subject: Value,
    pub approved: bool,
    /// The decision's issuer holds review authority for this project.
    pub issuer_authorized: bool,
}

pub enum EvidenceRef {
    Check(Uuid),
    Judgment(Uuid),
    Work,
    Review(Uuid),
    Unknown,
}
impl EvidenceRef {
    pub fn parse(id: &str) -> Self {
        if let Some(op) = handle(id, CHECK_EVIDENCE) {
            return Self::Check(op);
        }
        if let Some(op) = handle(id, JUDGMENT_EVIDENCE) {
            return Self::Judgment(op);
        }
        if id.starts_with(WORK_REF) {
            return Self::Work;
        }
        handle(id, REVIEW_EVIDENCE)
            .map(Self::Review)
            .unwrap_or(Self::Unknown)
    }
}

type Verdict<T> = std::result::Result<T, &'static str>;
pub struct EvidenceSet<'a> {
    policy: &'a EvidencePolicy,
    evidence: &'a Value,
    candidate: &'a str,
    source: Value,
    passed_checks: BTreeSet<String>,
    judged: bool,
    reviewed: bool,
}
impl<'a> EvidenceSet<'a> {
    pub fn new(
        policy: &'a EvidencePolicy,
        evidence: &'a Value,
        candidate: &'a str,
        source: Value,
    ) -> Self {
        Self {
            policy,
            evidence,
            candidate,
            source,
            passed_checks: BTreeSet::new(),
            judged: false,
            reviewed: false,
        }
    }
    pub fn references(&self) -> Verdict<Vec<&'a str>> {
        let ids: Vec<&str> = self
            .evidence
            .as_array()
            .and_then(|items| items.iter().map(|e| e["id"].as_str()).collect())
            .ok_or("invalid_request")?;
        let unique: BTreeSet<_> = ids.iter().collect();
        if ids.len() > MAX_EVIDENCE || unique.len() != ids.len() {
            return Err("invalid_request");
        }
        Ok(ids)
    }
    pub fn check(&mut self, row: Option<PgRow>) -> Result<Verdict<()>> {
        let Some(row) = row else {
            return Ok(Err("evidence_unknown"));
        };
        let (status, request, check_id): (String, Value, Option<String>) =
            (row.get("status"), row.get("request"), row.get("check_id"));
        if status == "running" {
            return Ok(Err("evidence_pending"));
        }
        if request["execution_profile"].as_str() != Some(CHECK_EXECUTION_PROFILE) {
            return Ok(Err("evidence_unauthorized"));
        }
        if !self.matches_check(&request) {
            return Ok(Err("evidence_stale"));
        }
        if status != "passed" {
            return Ok(Err("evidence_failed"));
        }
        self.passed_checks.insert(check_id.ok_or(Error::Unknown)?);
        Ok(Ok(()))
    }
    fn matches_check(&self, request: &Value) -> bool {
        let source = &request["source"];
        let oid = (&source["format"], &source["hex"]);
        same_subject(&request["subject"], self.candidate, 1)
            && oid == (&self.source["format"], &self.source["hex"])
    }
    pub fn judgment(&mut self, row: Option<PgRow>) -> Verdict<()> {
        let row = row.ok_or("evidence_unknown")?;
        if self.policy.judgment_choices.is_empty() {
            return Err("evidence_unauthorized");
        }
        let (status, request, output): (String, Value, Option<Value>) =
            (row.get("status"), row.get("request"), row.get("output"));
        if status == "running" {
            return Err("evidence_pending");
        }
        if !same_subject(&request["subject"], self.candidate, 1) {
            return Err("evidence_stale");
        }
        let output = output.unwrap_or(Value::Null);
        if status == "assessed" && !matching_credential(&request, &output) {
            return Err("evidence_unauthorized");
        }
        let choice = output["choice"].as_str().unwrap_or("");
        if status != "assessed" || !self.policy.judgment_choices.iter().any(|c| c == choice) {
            return Err("evidence_failed");
        }
        self.judged = true;
        Ok(())
    }
    pub async fn receipt(
        db: &mut PgConnection,
        namespace: &Namespace,
        operation: Uuid,
        command: &str,
    ) -> Result<Option<sqlx::postgres::PgRow>> {
        Ok(sqlx::query("SELECT status,request,output,check_id FROM playbook_host_operations WHERE operation_id=$1 AND namespace_id=$2 AND generation=$3 AND command_name=$4")
            .bind(operation)
            .bind(namespace.namespace_id)
            .bind(namespace.generation)
            .bind(command)
            .fetch_optional(db)
            .await?)
    }

    pub async fn work_row(
        &self,
        db: &mut PgConnection,
        namespace: &Namespace,
        id: &str,
    ) -> Result<Option<PgRow>> {
        Ok(sqlx::query("SELECT status,request,output FROM playbook_host_operations WHERE namespace_id=$1 AND generation=$2 AND command_name='work.ensure' AND request->>'reference'=$3").bind(namespace.namespace_id).bind(namespace.generation).bind(id).fetch_optional(db).await?)
    }
    pub fn work(&mut self, row: Option<PgRow>) -> Verdict<()> {
        let row = row.ok_or("evidence_unknown")?;
        let (status, request, output): (String, Value, Option<Value>) =
            (row.get("status"), row.get("request"), row.get("output"));
        if !matches!(request["kind"].as_str(), Some("review" | "fidelity_review")) {
            return Err("evidence_unauthorized");
        }
        if status == "running" {
            return Err("evidence_pending");
        }
        if !same_subject(&request["motivation"], self.candidate, 1) {
            return Err("evidence_stale");
        }
        if status != "completed"
            || output.as_ref().map(|o| &o["disposition"]) != Some(&json!("accepted"))
        {
            return Err("evidence_failed");
        }
        self.reviewed = true;
        Ok(())
    }
    pub fn review(&mut self, review: Option<ReviewEvidence>) -> Verdict<()> {
        let review = review.ok_or("evidence_unknown")?;
        if !review.issuer_authorized {
            return Err("evidence_unauthorized");
        }
        if !same_subject(&review.subject, self.candidate, 1) {
            return Err("evidence_stale");
        }
        if !review.approved {
            return Err("evidence_failed");
        }
        self.reviewed = true;
        Ok(())
    }
    pub fn finish(&self, ids: Vec<&str>) -> Verdict<Vec<String>> {
        let checks = self
            .policy
            .checks
            .iter()
            .all(|c| self.passed_checks.contains(c));
        let review = !self.policy.review || self.reviewed;
        let judgment = self.policy.judgment_choices.is_empty() || self.judged;
        if [checks, review, judgment].contains(&false) {
            return Err("evidence_missing");
        }
        Ok(ids.into_iter().map(str::to_string).collect())
    }
}

fn matching_credential(request: &Value, output: &Value) -> bool {
    let profile = request["credential_profile"]
        .as_str()
        .filter(|value| !value.is_empty());
    profile.is_some() && profile == output["provenance"]["credential_profile"].as_str()
}

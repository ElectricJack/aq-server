//! Verify a trusted health observation and derive ordered finding episode changes.
use crate::{
    Error, Namespace, Result,
    health_scans::bounded,
    host_values::{HostResult, subject, text},
};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row, postgres::PgRow};
use std::collections::{BTreeMap, BTreeSet};

pub struct Finding<'a> {
    key: &'a str,
    evidence: &'a str,
    expected: Option<i64>,
}
pub struct FindingScan<'a> {
    pub partition: &'a str,
    observation: &'a str,
    evaluation: &'a str,
    pub findings: Vec<Finding<'a>>,
    seen: BTreeSet<&'a str>,
    now: i64,
    reminder: i64,
}
impl<'a> FindingScan<'a> {
    pub fn parse(input: &'a Value, clock: impl FnOnce() -> Result<i64>) -> Result<Self> {
        let partition = text(input, "partition")?;
        let observation = text(input, "observation")?;
        let evaluation = text(input, "evaluation")?;
        if ![partition, observation, evaluation]
            .into_iter()
            .all(bounded)
        {
            return Err(Error::Invalid);
        }
        let items = input["findings"].as_array().ok_or(Error::Invalid)?;
        let now = clock()?;
        let reminder = input["reminder_ms"]
            .as_i64()
            .filter(|v| *v >= 0)
            .ok_or(Error::Invalid)?;
        let mut seen = BTreeSet::new();
        let mut findings = Vec::with_capacity(items.len());
        for item in items {
            let key = text(item, "key")?;
            if !bounded(key) {
                return Err(Error::Invalid);
            }
            let evidence = text(item, "evidence")?;
            if !(1..=4096).contains(&evidence.len()) || evidence.contains('\0') {
                return Err(Error::Invalid);
            }
            if !seen.insert(key) {
                return Err(Error::Invalid);
            }
            findings.push(Finding {
                key,
                evidence,
                expected: item["expected_revision"].as_i64(),
            });
        }
        Ok(Self {
            partition,
            observation,
            evaluation,
            findings,
            seen,
            now,
            reminder,
        })
    }
    pub fn result(&self, outcome: &str) -> HostResult {
        HostResult {
            outcome: outcome.into(),
            output: json!({"reference":self.observation,"subject":subject(self.partition,0)}),
        }
    }
    pub async fn observation(
        &self,
        db: &mut PgConnection,
        ns: &Namespace,
    ) -> Result<Option<PgRow>> {
        Ok(sqlx::query("SELECT * FROM playbook_health_observations WHERE namespace_id=$1 AND generation=$2 AND observation=$3")
        .bind(ns.namespace_id).bind(ns.generation).bind(self.observation).fetch_optional(db).await?)
    }
}
pub struct RetainedScan<'a> {
    pub namespace: &'a Namespace,
    pub row: PgRow,
    pub hash: String,
}
impl RetainedScan<'_> {
    pub fn retained(&self, request: &FindingScan<'_>) -> Result<Option<HostResult>> {
        let Some(old) = self.row.get::<Option<Value>, _>("result") else {
            return Ok(None);
        };
        if self
            .row
            .get::<Option<String>, _>("request_digest")
            .as_deref()
            != Some(&self.hash)
        {
            return Ok(Some(request.result("conflict")));
        }
        Ok(Some(HostResult {
            outcome: old["outcome"].as_str().ok_or(Error::Unknown)?.into(),
            output: old["output"].clone(),
        }))
    }
    pub fn permits(
        &self,
        request: &FindingScan<'_>,
        clock: impl FnOnce() -> Result<i64>,
    ) -> Result<bool> {
        let same_partition = self.row.get::<String, _>("partition") == request.partition;
        Ok(same_partition
            && self.row.get::<Option<String>, _>("evaluation").as_deref()
                == Some(request.evaluation)
            && self.row.get::<i64, _>("expires_at_ms") > clock()?)
    }
    pub async fn episodes(&self, db: &mut PgConnection, partition: &str) -> Result<Episodes> {
        let baseline = serde_json::from_value(self.row.get("baseline"))?;
        let rows=sqlx::query("SELECT finding_key,revision,present,evidence,last_notified_ms FROM playbook_finding_episodes WHERE namespace_id=$1 AND generation=$2 AND partition=$3")
         .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(partition).fetch_all(db).await?;
        let current = rows
            .into_iter()
            .map(|r| (r.get("finding_key"), Episode::from_row(r)))
            .collect();
        Ok(Episodes {
            baseline,
            current,
            complete: self.row.get("complete"),
        })
    }
    pub async fn record(
        &self,
        db: &mut PgConnection,
        request: &FindingScan<'_>,
        result: &HostResult,
    ) -> Result<()> {
        sqlx::query("UPDATE playbook_health_observations SET request_digest=$4,result=$5 WHERE namespace_id=$1 AND generation=$2 AND observation=$3")
        .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(request.observation).bind(&self.hash).bind(json!({"outcome":result.outcome,"output":result.output})).execute(db).await?;
        Ok(())
    }
}
struct Episode {
    revision: i64,
    present: bool,
    evidence: String,
    last_notified: i64,
}
impl Episode {
    fn from_row(row: PgRow) -> Self {
        Self {
            revision: row.get("revision"),
            present: row.get("present"),
            evidence: row.get("evidence"),
            last_notified: row.get("last_notified_ms"),
        }
    }
    fn should_notify(&self, evidence: &str, request: &FindingScan<'_>) -> bool {
        let reminder_due = request.reminder > 0
            && request.now.saturating_sub(self.last_notified) >= request.reminder;
        !self.present || self.evidence != evidence || reminder_due
    }
}
pub struct Episodes {
    baseline: BTreeMap<String, i64>,
    current: BTreeMap<String, Episode>,
    complete: bool,
}
pub struct EpisodeChange<'a> {
    pub key: &'a str,
    pub revision: i64,
    pub present: bool,
    pub evidence: &'a str,
    pub notify: bool,
}
fn next_revision(old: i64) -> Result<i64> {
    old.checked_add(1).ok_or(Error::Overloaded)
}
impl Episodes {
    pub fn conflicts(&self, request: &FindingScan<'_>) -> bool {
        request.findings.iter().any(|f| !self.matches(f))
            || (self.complete
                && self
                    .current
                    .iter()
                    .any(|(key, e)| self.baseline.get(key) != Some(&e.revision)))
    }
    fn matches(&self, finding: &Finding<'_>) -> bool {
        let key = finding.key;
        let baseline = self.baseline.get(key).copied().unwrap_or(0);
        let current = self.current.get(key).map_or(0, |old| old.revision);
        finding.expected == Some(baseline) && finding.expected == Some(current)
    }
    pub fn reported<'a>(
        &self,
        finding: &Finding<'a>,
        scan: &FindingScan<'_>,
    ) -> Result<EpisodeChange<'a>> {
        let old = self.current.get(finding.key);
        Ok(EpisodeChange {
            key: finding.key,
            revision: next_revision(old.map_or(0, |e| e.revision))?,
            present: true,
            evidence: finding.evidence,
            notify: old.is_none_or(|e| e.should_notify(finding.evidence, scan)),
        })
    }
    pub fn cleared<'a>(
        &'a self,
        request: &'a FindingScan<'_>,
    ) -> impl Iterator<Item = Result<EpisodeChange<'a>>> {
        self.current
            .iter()
            .filter(move |(key, old)| {
                self.complete && old.present && !request.seen.contains(key.as_str())
            })
            .map(|(key, old)| {
                Ok(EpisodeChange {
                    key,
                    revision: next_revision(old.revision)?,
                    present: false,
                    evidence: &old.evidence,
                    notify: true,
                })
            })
    }
}

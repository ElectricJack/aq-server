use super::*;
use aq_playbook_domain::context_reads::{ContextQuery, Observation};

impl Service {
    /// Typed views observed in one transaction and retained per intent.
    pub(super) async fn context_read(
        &self,
        owner: &RuntimeOwner,
        command: HostIntent<'_>,
    ) -> Result<HostResult> {
        let intent = command.intent;
        let ports = self.ports()?;
        let (mut tx, stage, identity) = self.begin(Some(owner), command).await?;
        match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                return Ok(result);
            }
            Stage::Pending(_) => return Err(Error::Unknown),
            Stage::Fresh => {}
        }
        let query = ContextQuery::parse(&intent.input)?;
        let now = self.now()?;
        let kind = text(&intent.input, "view")?;
        let view = match kind {
            "work" => self.work_observation(&mut tx, ports, query.subject).await?,
            "health_observations" if self.profile.has_findings() => {
                self.health_observation(&mut tx, intent, &query).await?
            }
            _ => query.read(&mut tx, self.namespace.project_id, kind).await?,
        };
        let result = view.result(&query, now);
        self.settle_fresh(tx, command.retained(&identity), result)
            .await
    }

    async fn work_observation(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ports: &HostPorts,
        subject: &str,
    ) -> Result<Observation> {
        let Ok(task) = Uuid::parse_str(subject) else {
            return Ok(Observation::refused(subject));
        };
        if !self.namespace_work(tx, task).await? {
            return Ok(Observation::refused(subject));
        }
        let reference = format!("{WORK_REF}{task}");
        match ports.graph.node(self.namespace.workspace_id, task).await? {
            Some(node) => {
                let revision = node_version(&node)?;
                Ok(Observation::observed(
                    reference,
                    revision,
                    format!("v{revision}"),
                ))
            }
            None => Ok(Observation::refused(reference)),
        }
    }
    async fn health_observation(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        intent: &Intent,
        query: &ContextQuery<'_>,
    ) -> Result<Observation> {
        if intent.input["page"].as_u64() != Some(0) {
            return Ok(Observation::incomplete(query.subject));
        }
        let reference = format!("aq.health/{}", self.operation(intent)?);
        let scan = self
            .capture_health_scan(tx, &reference, query.subject, 60_000)
            .await?;
        let snapshot = serde_json::to_string(&scan)?;
        if snapshot.len() > 4096 {
            return Ok(Observation::incomplete(reference));
        }
        Ok(Observation::observed(reference, 0, snapshot))
    }
}

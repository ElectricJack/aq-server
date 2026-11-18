//! Playbooks view read contracts (G11 X30-X33), additive on
//! playbooks-inspection-1: definition name and version, pinned-versus-active
//! comparison, the ordered public step graph and the pending human decision.
//! Read-only: no activation, install or decision authority.
use super::{RunView, catalog};
use crate::playbooks::{
    domain::{Error, Result},
    driver::Driver,
    service::Service,
};
use aq_playbook_host::{model::Artifact, runner};
use sqlx::Row;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
use uuid::Uuid;

const GRAPH_BYTES: usize = 65536;
const ACTIVATION_PAGES: usize = 4;
/// Per-page budget for reading current activations. 500 ms was too tight on
/// loaded CI runners once the 2.1.0 catalog landed: the read timed out and the
/// run's definition reported `active`/`pinned_is_active` as unknown.
const ACTIVATION_PAGE_BUDGET: Duration = Duration::from_secs(2);

pub use aq_playbook_domain::playbook_view::*;
impl Service {
    /// Idempotent: the first install of an artifact in this namespace
    /// generation fixes its per-name version.
    pub(crate) async fn register_definition(&self, artifact: &Artifact) -> Result<()> {
        let graph = serde_json::to_value(graph(&artifact.content.playbook))?;
        let graph = (serde_json::to_vec(&graph)?.len() <= GRAPH_BYTES).then_some(graph);
        let mut tx = self.tx().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(format!(
                "aq.playbook.definitions/{}/{}",
                self.namespace.namespace_id, self.namespace.generation
            ))
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO playbook_definitions(namespace_id,generation,artifact,name,version,graph,registered_at_ms) SELECT $1,$2,$3,$4,COALESCE(MAX(version),0)+1,$5,$6 FROM playbook_definitions WHERE namespace_id=$1 AND generation=$2 AND name=$4 ON CONFLICT (namespace_id,generation,artifact) DO NOTHING")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(&artifact.digest)
            .bind(&artifact.content.playbook.id)
            .bind(graph)
            .bind(self.current_time())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    async fn registered(&self, artifacts: &[String]) -> Result<BTreeMap<String, (String, i64)>> {
        let mut tx = self.tx().await?;
        let rows = sqlx::query("SELECT artifact,name,version FROM playbook_definitions WHERE namespace_id=$1 AND generation=$2 AND artifact=ANY($3)")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(artifacts)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|row| (row.get("artifact"), (row.get("name"), row.get("version"))))
            .collect())
    }
    async fn stored_graph(&self, artifact: &str) -> Result<Option<Graph>> {
        let mut tx = self.tx().await?;
        let graph: Option<Option<serde_json::Value>> = sqlx::query_scalar("SELECT graph FROM playbook_definitions WHERE namespace_id=$1 AND generation=$2 AND artifact=$3")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(artifact)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        graph
            .flatten()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| Error::Unknown)
    }
    /// Binding and activation revision of runs admitted through a retained
    /// admission. Binding is null for admissions prepared before it was kept.
    async fn admitted_runs(
        &self,
        runs: &[String],
    ) -> Result<BTreeMap<String, (Option<String>, i64)>> {
        if runs.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut tx = self.tx().await?;
        let rows = sqlx::query("SELECT receipt->>'run_id' AS run_id,binding,expected_activation_revision FROM playbook_admissions WHERE namespace_id=$1 AND generation=$2 AND receipt IS NOT NULL AND receipt->>'run_id'=ANY($3)")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(runs)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    row.get("run_id"),
                    (row.get("binding"), row.get("expected_activation_revision")),
                )
            })
            .collect())
    }
    async fn pending_decisions(&self, intents: &[String]) -> Result<BTreeMap<String, WaitingView>> {
        if intents.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut tx = self.tx().await?;
        let rows = sqlx::query("SELECT e.intent_id,e.receipt->>'handle' AS handle,d.state,s.task_id FROM playbook_effects e LEFT JOIN human_decision_requests d ON d.decision_id::text=e.receipt->>'handle' LEFT JOIN review_subjects s ON s.subject_digest=d.subject_digest WHERE e.namespace_id=$1 AND e.generation=$2 AND e.command_name='decision.request' AND e.intent_id=ANY($3)")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(intents)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        let mut out = BTreeMap::new();
        for row in rows {
            let intent: String = row.get("intent_id");
            // A queued or refused request has no decision handle yet: the run
            // is not waiting on a human decision, so no view (never an error
            // that would hide the whole runs page).
            let Some(decision_id) = row
                .get::<Option<String>, _>("handle")
                .and_then(|h| Uuid::parse_str(&h).ok())
            else {
                continue;
            };
            out.insert(
                intent.clone(),
                WaitingView {
                    kind: "decision".into(),
                    intent,
                    decision_id,
                    decision_state: row.get("state"),
                    related_task_ids: row.get::<Option<Uuid>, _>("task_id").into_iter().collect(),
                },
            );
        }
        Ok(out)
    }
}
impl Driver {
    /// Every current activation, or None when the catalog is unsupported,
    /// unreadable or larger than this bounded observation.
    async fn current_activations(&self) -> Option<Vec<catalog::Activation>> {
        if !self.catalog_supported() {
            return None;
        }
        let mut out = Vec::new();
        let mut after = None;
        for _ in 0..ACTIVATION_PAGES {
            let query = runner::CatalogQuery {
                kind: runner::CatalogKind::Activations,
                exact: None,
                after: after.take(),
                limit: 25,
                expected: None,
            };
            let raw = self
                .owner
                .inspect_catalog(query.clone(), ACTIVATION_PAGE_BUDGET)
                .await
                .ok()?;
            let page = catalog::public_page(
                &self.service.namespace,
                catalog::Kind::Activations,
                &query,
                raw,
                self.service.current_time(),
            )
            .ok()?;
            out.extend(page.items.into_iter().filter_map(|item| match item.record {
                catalog::Record::Activation(a) => Some(a),
                catalog::Record::Definition(_) => None,
            }));
            match page.next_cursor {
                None => return Some(out),
                Some(cursor) => after = Some(cursor),
            }
        }
        None
    }
    /// Adds X30/X31 definition and X33 waiting views to already authorized
    /// public run projections.
    pub(super) async fn enrich_runs(&self, runs: &mut [RunView]) -> Result<()> {
        if runs.is_empty() {
            return Ok(());
        }
        let activations = self.current_activations().await;
        let artifacts = runs
            .iter()
            .map(|r| r.artifact.clone())
            .chain(activations.iter().flatten().map(|a| a.artifact.clone()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let registry = self.service.registered(&artifacts).await?;
        let intents = runs
            .iter()
            .flat_map(|r| r.pending_intents.iter().cloned())
            .collect::<Vec<_>>();
        let waiting = self.service.pending_decisions(&intents).await?;
        let ids = runs.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
        let admitted = self.service.admitted_runs(&ids).await?;
        for run in runs.iter_mut() {
            if let Some((binding, revision)) = admitted.get(&run.id) {
                run.binding = binding.clone();
                run.activation_revision = Some(revision.to_string());
            }
            run.waiting = run
                .pending_intents
                .iter()
                .find_map(|intent| waiting.get(intent).cloned());
            run.definition = registry
                .get(&run.artifact)
                .map(|(name, version)| definition(run, name, *version, &activations, &registry));
        }
        Ok(())
    }
    pub(super) async fn step_timeline(&self, run: &RunView) -> Result<Option<StepTimeline>> {
        Ok(self
            .service
            .stored_graph(&run.artifact)
            .await?
            .and_then(|graph| ordered(&graph, &run.entry)))
    }
}
fn definition(
    run: &RunView,
    name: &str,
    version: i64,
    activations: &Option<Vec<catalog::Activation>>,
    registry: &BTreeMap<String, (String, i64)>,
) -> DefinitionView {
    let candidates = activations.iter().flatten().filter_map(|a| {
        registry
            .get(&a.artifact)
            .filter(|(n, _)| n == name)
            .map(|(_, v)| (a, *v))
    });
    let active = candidates
        .max_by(|(a, av), (b, bv)| {
            let own = |x: &catalog::Activation| run.binding.as_ref() == Some(&x.binding);
            (own(a), a.artifact == run.artifact, *av, &b.binding).cmp(&(
                own(b),
                b.artifact == run.artifact,
                *bv,
                &a.binding,
            ))
        })
        .map(|(a, v)| ActiveDefinitionView {
            binding: a.binding.clone(),
            artifact: a.artifact.clone(),
            version: v.to_string(),
            activation_revision: a.revision.clone(),
        });
    DefinitionView {
        name: name.into(),
        version: version.to_string(),
        pinned_is_active: activations
            .as_ref()
            .map(|_| active.as_ref().is_some_and(|a| a.artifact == run.artifact)),
        active,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn run_view(artifact: &str) -> RunView {
        RunView {
            id: "r".into(),
            artifact: artifact.into(),
            entry: "main".into(),
            step: "s".into(),
            status: "waiting".into(),
            outcome: None,
            reason: None,
            root: "r".into(),
            parent: None,
            children: vec![],
            dependencies: BTreeMap::new(),
            pending_kind: None,
            admitted_at_ms: 1,
            definition: None,
            waiting: None,
            binding: None,
            activation_revision: None,
            pending_intents: vec![],
        }
    }
    fn activation(binding: &str, artifact: &str) -> catalog::Activation {
        catalog::Activation {
            binding: binding.into(),
            artifact: artifact.into(),
            revision: "3".into(),
            controls: catalog::Controls {
                calls_enabled: true,
                event_starts_enabled: true,
                execution_enabled: true,
            },
            artifact_execution_denied: false,
        }
    }
    #[test]
    fn definition_view_compares_the_pinned_artifact_with_the_current_activation() {
        let registry = BTreeMap::from([
            ("a1".to_string(), ("recovery".to_string(), 1)),
            ("a2".to_string(), ("recovery".to_string(), 2)),
            ("x".to_string(), ("other".to_string(), 7)),
        ]);
        let superseded = definition(
            &run_view("a1"),
            "recovery",
            1,
            &Some(vec![activation("main", "a2"), activation("o", "x")]),
            &registry,
        );
        assert_eq!(superseded.version, "1");
        assert_eq!(superseded.pinned_is_active, Some(false));
        let active = superseded.active.unwrap();
        assert_eq!(
            (active.artifact.as_str(), active.version.as_str()),
            ("a2", "2")
        );
        let current = definition(
            &run_view("a2"),
            "recovery",
            2,
            &Some(vec![activation("main", "a2")]),
            &registry,
        );
        assert_eq!(current.pinned_is_active, Some(true));
        let retired = definition(&run_view("a1"), "recovery", 1, &Some(vec![]), &registry);
        assert_eq!(
            (retired.pinned_is_active, retired.active),
            (Some(false), None)
        );
        let unknown = definition(&run_view("a1"), "recovery", 1, &None, &registry);
        assert_eq!((unknown.pinned_is_active, unknown.active), (None, None));
    }
}

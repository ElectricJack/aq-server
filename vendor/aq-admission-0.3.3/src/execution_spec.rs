//! Typed immutable worker artifact. This is neither a legacy ExecutionSpec nor
//! an execution grant. It contains references and provenance, never secret bytes.
use crate::*;
use aq_lifecycle_protocol::Binding;
use aq_session_protocol::ContextDelivery;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfiledExecutionSpec {
    pub schema_version: u32,
    pub lifecycle: Binding,
    pub project_revision: u64,
    pub task: TaskRevision,
    pub source: SourceAdmission,
    pub source_artifact: ArtifactRef,
    pub outcome_artifact: ArtifactRef,
    pub graph_access_profile: ArtifactRef,
    pub task_observation: ArtifactRef,
    pub task_context: ArtifactRef,
    pub selection_digest: Digest,
}
impl ProfiledExecutionSpec {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "profiled spec version")?;
        self.lifecycle
            .validate()
            .map_err(|_| Invalid("profiled spec lifecycle"))?;
        require(
            matches!(self.lifecycle.session, SessionIdentity::Remote { .. }),
            "profiled worker spec requires remote identity",
        )?;
        revision(self.project_revision)?;
        self.task.validate()?;
        require(
            self.lifecycle.task_id == self.task.task_id,
            "spec task identity",
        )?;
        require(
            self.lifecycle.lineage_id
                == transport::task_lineage_id(
                    self.lifecycle.project_id,
                    self.task.workspace_id,
                    self.lifecycle.task_id,
                )?,
            "server-owned spec lineage",
        )?;
        self.source.validate()?;
        require(
            self.source.project_id == self.lifecycle.project_id
                && self.source.project_revision == self.project_revision,
            "spec source project",
        )?;
        for reference in [
            &self.source_artifact,
            &self.outcome_artifact,
            &self.graph_access_profile,
            &self.task_observation,
            &self.task_context,
        ] {
            artifact(reference)?;
        }
        Ok(())
    }
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        aq_session_protocol::canonical_bytes(self)
            .map_err(|_| Invalid("typed execution spec encoding"))
    }
    pub fn artifact(&self) -> Result<ArtifactRef> {
        self.validate()?;
        Ok(ArtifactRef {
            id: Text::new("aq.profiled-execution-spec")
                .map_err(|_| Invalid("spec artifact identity"))?,
            version: Text::new("1").map_err(|_| Invalid("spec artifact version"))?,
            digest: aq_session_protocol::canonical_digest(self)
                .map_err(|_| Invalid("typed execution spec digest"))?,
        })
    }
    pub fn validate_retained(
        &self,
        offer: &ProfiledOffer,
        artifacts: &[RetainedArtifact],
        now: u64,
    ) -> Result<()> {
        self.validate_against(offer)?;
        verify_offer_artifacts(offer, artifacts, now)?;
        let own_artifact = self.artifact()?;
        let canonical_size = self.canonical_bytes()?.len() as u64;
        require(
            artifacts.iter().any(|artifact| {
                artifact.artifact == own_artifact && artifact.bytes == canonical_size
            }),
            "typed spec canonical byte descriptor",
        )?;
        for (reference, kind) in [
            (&own_artifact, ArtifactKind::Composition),
            (&self.task_observation, ArtifactKind::Context),
            (&self.task_context, ArtifactKind::Context),
        ] {
            require(
                artifacts
                    .iter()
                    .any(|artifact| artifact.artifact == *reference && artifact.kind == kind),
                "retained typed spec/task artifact role",
            )?;
        }
        Ok(())
    }
    pub fn validate_against(&self, offer: &ProfiledOffer) -> Result<()> {
        self.validate()?;
        offer
            .validate()
            .map_err(|_| Invalid("profiled spec offer"))?;
        let binding = &offer.manifest.bindings;
        require(
            self.lifecycle == transport::lifecycle_binding(offer)?
                && self.project_revision == binding.project_revision
                && self.task.workspace_id == binding.graph_id
                && self.task.version == binding.task_revision
                && self.source_artifact == binding.source
                && self.outcome_artifact == binding.outcome
                && self.graph_access_profile == binding.graph_access_profile,
            "exact spec launch bindings",
        )?;
        require(
            offer.manifest.context.references.iter().any(|context| {
                context.artifact == self.task_context
                    && context.required
                    && context.delivery == ContextDelivery::Startup
            }),
            "required task startup context",
        )?;
        require(
            aq_session_protocol::canonical_digest(self)
                .map_err(|_| Invalid("profiled spec digest"))?
                == offer.binding.execution_spec_digest,
            "typed execution spec digest",
        )
    }
}
/// The byte limit applies before parsing. Exact secret-free fields and duplicate
/// field rejection are enforced by typed deserialization before validation.
pub fn decode_profiled_spec(bytes: &[u8]) -> Result<ProfiledExecutionSpec> {
    require(bytes.len() <= 262_144, "profiled spec artifact bound")?;
    let spec: ProfiledExecutionSpec =
        serde_json::from_slice(bytes).map_err(|_| Invalid("profiled spec JSON shape"))?;
    spec.validate()?;
    Ok(spec)
}
impl aq_session_protocol::Validate for ProfiledExecutionSpec {
    fn validate(&self) -> std::result::Result<(), aq_session_protocol::ContractError> {
        ProfiledExecutionSpec::validate(self)
            .map_err(|error| aq_session_protocol::ContractError::Invalid(error.0))
    }
}

pub fn execution_spec_schema() -> serde_json::Value {
    let mut schema =
        serde_json::to_value(schemars::schema_for!(ProfiledExecutionSpec)).expect("spec schema");
    let base = crate::contract_schema();
    if let Some(defs) = schema
        .get_mut("$defs")
        .and_then(serde_json::Value::as_object_mut)
    {
        for (name, definition) in defs.iter_mut() {
            if let Some(bounded) = base.get("$defs").and_then(|defs| defs.get(name)) {
                *definition = bounded.clone();
            }
        }
    }
    schema["$comment"] = serde_json::json!(
        "Artifact JSON <=262144 bytes. Relational lineage/session/project/source/task/manifest/digest checks are enforced by validate_against; schema alone conveys no authority. task.version is graph CAS version; task.content_revision is retained independently."
    );
    schema
}

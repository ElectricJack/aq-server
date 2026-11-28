use super::*;
pub(super) struct Selected {
    selection: SourceSelection,
    observed: u64,
    retention_handle: Uuid,
    evidence: BTreeMap<&'static str, Value>,
}
impl Selected {
    pub(super) fn new(policy: &ProjectPolicyRecord, fixed: Option<Fixed>, observed: u64) -> Self {
        let mut evidence = BTreeMap::new();
        let selection = fixed.as_ref().map_or_else(
            || policy.source.clone(),
            |fixed| SourceSelection::Immutable {
                object: ObjectId {
                    format: ObjectFormat::Sha1,
                    hex: fixed.hex.clone(),
                },
            },
        );
        // The export transfer (or handoff candidate) is the retained byte owner.
        // The checkpoint handle originally used may have since been released.
        let retention_handle = fixed
            .as_ref()
            .map_or(policy.retention_handle, |f| f.retention_handle);
        if let Some((key, value)) = fixed.as_ref().and_then(|f| f.evidence.clone()) {
            evidence.insert(key, value);
        }
        Self {
            selection,
            observed,
            retention_handle,
            evidence,
        }
    }
    pub(super) async fn resolve(
        &mut self,
        reader: Observer<'_>,
        enrolled: bool,
        _offer: Option<&TaskContext>,
    ) -> Result<ObjectId, AppError> {
        let reference = match &self.selection {
            SourceSelection::Immutable { object } => return Ok(object.clone()),
            SourceSelection::NamedRef { reference } => reference.as_str().to_owned(),
        };
        // Spec 0016 §4: a named ref follows the head central aq-git last synced
        // from the remote and retained, synced now when older than Settings allow.
        if enrolled {
            let synced = sync_current(reader, &reference, _offer).await?;
            return self.apply_sync(reader, synced);
        }
        // D1: use the published source head and candidate handle if AQ published
        // this named ref. Without one, obtain a fresh repository observation.
        if !enrolled {
            let repository = reader.repository();
            let head = crate::profiled_execution::settlement::publication::current_head(
                &reader.state.store.pool,
                reader.project.project.binding.project_id,
                repository.id,
                repository.generation,
                &reference,
            )
            .await
            .map_err(|_| refused())?;
            if let Some(head) = head {
                self.retention_handle = head.retention_handle;
                self.evidence.insert(
                    "publication_head",
                    serde_json::to_value(&head).map_err(|_| AppError::Internal)?,
                );
                return Ok(sha1(head.oid));
            }
        }
        self.observe_ref(reader, &reference).await
    }
    async fn observe_ref(
        &mut self,
        reader: Observer<'_>,
        reference: &str,
    ) -> Result<ObjectId, AppError> {
        let repository = reader.repository();
        let response = read(
            reader.state,
            repository,
            "repo.observation",
            json!({"refs":[reference],"max_age_ms":30000}),
        )
        .await?;
        let received = reader.now()?;
        if received < self.observed {
            return Err(refused());
        }
        let object = resolve_ref(&response, reference, repository.generation, received)?;
        self.observed = self
            .observed
            .min(response["started_at_ms"].as_u64().ok_or_else(refused)?);
        self.evidence.insert("reference_observation", response);
        Ok(object)
    }
    pub(super) async fn inspect(
        &mut self,
        reader: Observer<'_>,
        object: &ObjectId,
    ) -> Result<(), AppError> {
        let repository = reader.repository();
        let inspected = read(
            reader.state,
            repository,
            "inspect",
            json!({"oid":{"algorithm":"sha1","hex":object.hex}}),
        )
        .await?;
        if !aq_delivery::source_checks::commit_matches(&inspected, &object.hex) {
            return Err(refused());
        }
        self.evidence.insert("commit_inspection", inspected);
        let retained = read(
            reader.state,
            repository,
            "retention.inspect",
            json!({"handle_id":self.retention_handle,"limit":1}),
        )
        .await?;
        if !aq_delivery::source_checks::retention_holds(
            &retained,
            self.retention_handle,
            repository,
            &object.hex,
        ) {
            return Err(refused());
        }
        self.evidence.insert("retention_inspection", retained);
        Ok(())
    }
    pub(super) fn observation(
        self,
        reader: Observer<'_>,
        object: ObjectId,
    ) -> Result<Observation, AppError> {
        let Self {
            observed,
            selection,
            retention_handle,
            evidence,
        } = self;
        let Observer {
            state: _,
            project,
            policy,
        } = reader;
        let binding = &project.project.binding;
        let now = reader.now()?;
        if now < observed || now >= observed.checked_add(30000).ok_or_else(|| refused())? {
            return Err(refused());
        }
        let admission = SourceAdmission {
            schema_version: 1,
            project_id: binding.project_id,
            project_revision: project.project.revision,
            repository_id: binding.repository_id,
            repository_binding_generation: binding.repository_binding_generation,
            policy: policy.artifact().map_err(|_| refused())?,
            selection,
            resolved_object: object,
            observed_at_ms: observed,
            expires_at_ms: observed + 30000,
        };
        admission.validate().map_err(|_| refused())?;
        let mut body = json!({"schema_version":1,"admission":admission,"retention_handle":retention_handle,"evidence":evidence});
        body.sort_all_objects();
        let bytes = serde_json::to_vec(&body).map_err(|_| AppError::Internal)?;
        if bytes.len() > 1_048_576 {
            return Err(refused());
        }
        let artifact =
            aq_delivery::source_checks::observation::artifact(&bytes).ok_or(AppError::Internal)?;
        Ok(Observation {
            admission,
            retention_handle,
            artifact,
            bytes,
        })
    }
    fn apply_sync(
        &mut self,
        reader: Observer<'_>,
        synced: aq_delivery::main_sync::MainSync,
    ) -> Result<ObjectId, AppError> {
        self.observed = u64::try_from((reader.state.clock)().timestamp_millis()).unwrap_or(0);
        if self.observed >= reader.project.binding_observation.expires_at_ms {
            return Err(refused());
        }
        self.retention_handle = synced.retention_handle;
        self.evidence.remove("reference_observation");
        self.evidence.remove("publication_head");
        self.evidence.insert(
            "main_sync",
            serde_json::to_value(&synced).map_err(|_| AppError::Internal)?,
        );
        Ok(sha1(synced.head))
    }
    pub(super) async fn verify_ancestry(
        &mut self,
        reader: Observer<'_>,
        offer: Option<&TaskContext>,
        object: ObjectId,
    ) -> Result<ObjectId, AppError> {
        let Some(offer) = offer.filter(|_| !self.evidence.contains_key("main_sync")) else {
            return Ok(object);
        };
        let state = reader.state;
        let binding = &reader.project.project.binding;
        use aq_delivery::main_sync::{OfferAncestry, Scope};
        let task = offer.revision.task_id;
        let project_id = binding.project_id;
        let repository = aq_delivery::commands::GitTarget {
            repository_id: binding.repository_id,
            binding_generation: binding.repository_binding_generation,
        };
        let moving = match &self.selection {
            SourceSelection::NamedRef { reference }
                if reference.as_str().starts_with("refs/heads/") =>
            {
                Some(Scope::new(
                    project_id,
                    repository.repository_id,
                    repository.binding_generation,
                    reference.as_str(),
                ))
            }
            _ => None,
        };
        let pool = &state.store.pool;
        let clock = || u64::try_from((state.clock)().timestamp_millis()).unwrap_or(0);
        let runtime = state.source_retention.as_ref().ok_or_else(refused)?;
        let follower =
            aq_delivery::main_sync::MainFollower::new(pool, &runtime.config.git_socket, clock);
        let verified = async {
            OfferAncestry::load(pool, repository, task, &offer.context)
                .await?
                .verify_or_refresh(&follower, &object.hex, moving.as_ref())
                .await
        }
        .await;
        match verified {
            Ok(None) => Ok(object),
            Ok(Some(synced)) => self.apply_sync(reader, synced),
            Err(wait) => Err(ancestry_wait(state, project_id, task, wait).await),
        }
    }
}
fn sha1(hex: String) -> ObjectId {
    ObjectId {
        format: ObjectFormat::Sha1,
        hex,
    }
}
async fn sync_current(
    reader: Observer<'_>,
    reference: &str,
    offer: Option<&TaskContext>,
) -> Result<aq_delivery::main_sync::MainSync, AppError> {
    let state = reader.state;
    let binding = &reader.project.project.binding;
    let runtime = state.source_retention.as_ref().ok_or_else(refused)?;
    let scope = aq_delivery::main_sync::Scope::new(
        binding.project_id,
        binding.repository_id,
        binding.repository_binding_generation,
        reference,
    );
    let clock = || u64::try_from((state.clock)().timestamp_millis()).unwrap_or(0);
    let (pool, socket) = (&state.store.pool, &runtime.config.git_socket);
    let follower = aq_delivery::main_sync::MainFollower::new(pool, socket, clock);
    if let Some(offer) = offer {
        let task = offer.revision.task_id;
        match follower
            .dependent_current(&scope, task, &offer.context)
            .await
        {
            Ok(synced) => Ok(synced),
            Err(wait) => Err(ancestry_wait(state, binding.project_id, task, wait).await),
        }
    } else {
        follower.current(&scope).await.map_err(|_| refused())
    }
}

use super::*;
pub(in crate::agent_access::human) struct OwnerRead<'a> {
    pub state: &'a AppState,
    pub runtime: &'a ReadRuntime,
    pub user: &'a User,
}
pub(in crate::agent_access::human) struct Claim<'a> {
    pub operation: Uuid,
    pub project: Uuid,
    pub entity: &'a str,
    pub expected: &'a str,
    pub tasks: &'a [Uuid],
}
impl Claim<'_> {
    fn subject(&self, request: &ActionRequest) -> ApiResult<Uuid> {
        if request.operation_id != self.operation || version(self.expected).is_none() {
            return Err(ApiError::invalid());
        }
        let subject = existing::subject(request, self.entity, self.entity, self.expected)?;
        if self.entity == "project" && subject != self.project {
            return Err(ApiError::invalid());
        }
        Ok(subject)
    }
}
impl OwnerRead<'_> {
    /// Recheck owner, current binding and subject under the Graph read policy.
    pub(in crate::agent_access::human) async fn admit(
        &self,
        request: &ActionRequest,
        claim: Claim<'_>,
    ) -> ApiResult<()> {
        let subject = claim.subject(request)?;
        existing::owner(self.state, self.user).await?;
        if claim.entity == "project" || request.kind == "task.delete" {
            access(self.state, self.runtime, self.user.clone(), claim.project)
                .await
                .map_err(ApiError::from)?;
        } else {
            let _ = detail(
                self.state,
                self.runtime,
                self.user.clone(),
                claim.project,
                subject,
                claim.entity,
                None,
            )
            .await
            .map_err(ApiError::from)?;
        }
        for task in claim.tasks {
            let _ = detail(
                self.state,
                self.runtime,
                self.user.clone(),
                claim.project,
                *task,
                "task",
                None,
            )
            .await
            .map_err(ApiError::from)?;
        }
        if !task_create_available(self.state) {
            return Err(ApiError::unavailable());
        }
        Ok(())
    }
    pub(in crate::agent_access::human) async fn lookup(
        &self,
        scope: &Scope,
        operation: Option<&actions::Operation>,
        entity: &str,
    ) -> ApiResult<()> {
        existing::owner(self.state, self.user).await?;
        let Scope::Project { project_id } = scope else {
            return Err(ApiError::invalid());
        };
        access(self.state, self.runtime, self.user.clone(), *project_id)
            .await
            .map_err(ApiError::from)?;
        verify_scope(operation, entity, *project_id)
    }
}
pub(super) fn verify_scope(
    operation: Option<&actions::Operation>,
    entity: &str,
    project: Uuid,
) -> ApiResult<()> {
    let Some(op) = operation else {
        return Ok(());
    };
    let [subject] = op.subject_refs.as_slice() else {
        return Err(ApiError::invalid());
    };
    let valid_project = entity != "project" || subject.id == project.to_string();
    if subject.entity_type != entity || !valid_project {
        return Err(ApiError::invalid());
    }
    Ok(())
}

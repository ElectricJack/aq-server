use super::*;
use aq_human_work::epic_retirement::{KIND, Register, owner_created};

impl GraphPayload for Register {
    const KIND: &'static str = KIND;
    const GRANT: &'static str = KIND;
    const ENTITY: &'static str = "task";
    fn operation_id(&self) -> Uuid {
        self.operation_id
    }
    fn project_id(&self) -> Uuid {
        self.project_id
    }
    fn expected(&self) -> &str {
        &self.expected_version
    }
    fn tasks(&self) -> Vec<Uuid> {
        vec![self.adopt.approval, self.adopt.cleanup]
    }
    fn valid(&self) -> bool {
        Register::valid(self)
    }
    fn operations(&self, _: &Current<'_>) -> ApiResult<Vec<Value>> {
        Err(ApiError::invalid())
    }
    fn retirement(&self) -> Option<&Register> {
        Some(self)
    }
}

async fn context(rt: &Runtime, workspace: Uuid, id: Uuid) -> Result<Value> {
    let result = rt
        .human_get(
            &format!("/v1/workspaces/{workspace}/nodes/{id}/context"),
            &[],
            262144,
        )
        .await?;
    if !contract::upstream_valid("Context", &result) {
        return Err(unavailable());
    }
    node(&result["node"], workspace, "task")?;
    Ok(result)
}

pub(super) async fn plan(
    state: &AppState,
    rt: &Runtime,
    workspace: Uuid,
    actor: Uuid,
    epic: &Value,
    payload: &Register,
) -> Result<Vec<Value>> {
    let service = rt.mutation_actor.as_deref().ok_or_else(unavailable)?;
    let pool = &state.store.pool;
    let (approval, cleanup, owned) = tokio::try_join!(
        context(rt, workspace, payload.adopt.approval),
        context(rt, workspace, payload.adopt.cleanup),
        async {
            owner_created(pool, actor, workspace, payload)
                .await
                .map_err(|_| unavailable())
        },
    )?;
    if !owned {
        return Err(invalid());
    }
    payload
        .plan(workspace, actor, service, epic, &approval, &cleanup)
        .ok_or_else(invalid)
}

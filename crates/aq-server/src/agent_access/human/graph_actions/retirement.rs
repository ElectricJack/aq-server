use super::*;
use aq_human_work::epic_retirement::{
    Register,
    dispatch::{self, *},
    settle::*,
};

/// Spec 0020 owner actions: the plan comes from `dispatch`, never `operations`.
macro_rules! retirement {
    ($($payload:ident $kind:literal $entity:literal;)*) => {$(
        impl GraphPayload for $payload {
            common!($kind, $entity, $kind);
            fn valid(&self) -> bool {
                $payload::valid(self)
            }
            fn tasks(&self) -> Vec<Uuid> {
                Action::$payload(self).tasks()
            }
            fn operations(&self, _: &Current<'_>) -> ApiResult<Vec<Value>> {
                Err(ApiError::invalid())
            }
            fn retirement(&self) -> Option<Action<'_>> {
                Some(Action::$payload(self))
            }
        }
    )*
        pub(super) fn register(state: &AppState, r: &mut Registry) {
            $(r.register(GraphAction::<$payload>::new(state)).expect($kind);)*
        }
        pub(super) fn schemas(r: &mut Registry) {
            $(r.register_schema::<GraphAction<$payload>>().expect($kind);)*
        }
    };
}
retirement! {
    Register "epic.retirement.register" "task";
    Restore "epic.retirement.restore" "knowledge";
    Decide "epic.retirement.decide" "knowledge";
    Evidence "epic.retirement.evidence" "knowledge";
    Close "epic.retirement.close" "knowledge";
}

impl Reads for Runtime {
    type Error = AppError;
    async fn get(&self, path: String, schema: &'static str) -> Result<Value> {
        let raw = self.human_get(&path, &[], 1_000_000).await?;
        contract::upstream_valid(schema, &raw)
            .then_some(raw)
            .ok_or_else(unavailable)
    }
    fn fail(&self, refusal: Option<&'static str>) -> AppError {
        tracing::info!(?refusal, "epic retirement action refused or unavailable");
        refusal.map_or_else(unavailable, |_| invalid())
    }
}

pub(super) async fn plan(
    state: &AppState,
    rt: &Runtime,
    (workspace, actor, project): (Uuid, Uuid, Uuid),
    subject: &Value,
    action: Action<'_>,
) -> Result<Vec<Value>> {
    let service = rt.mutation_actor.as_deref().ok_or_else(unavailable)?;
    let at = (workspace, actor, project, service);
    dispatch::plan(rt, &state.store.pool, at, subject, action).await
}

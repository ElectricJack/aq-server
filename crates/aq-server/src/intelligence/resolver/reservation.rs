//! Admission fences for an owned reservation and exact initial-grant replay.
use super::{Result, *};
use crate::execution::{
    authority::{self, AuthorityView, Kind, Policy, hot::HotState},
    profiled::Status,
};
use aq_session_protocol::LaunchBinding;
type Tx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

pub(crate) struct Reservation<'a> {
    binding: &'a LaunchBinding,
    grant_id: Option<Uuid>,
}
impl<'a> Reservation<'a> {
    pub(super) fn ungranted(binding: &'a LaunchBinding) -> Self {
        Self {
            binding,
            grant_id: None,
        }
    }
    /// The caller has verified the immutable Prepare receipt and original grant identity.
    pub(crate) fn grant_replay(binding: &'a LaunchBinding, id: Uuid) -> Self {
        Self {
            binding,
            grant_id: Some(id),
        }
    }
    pub(crate) async fn fence(
        &self,
        state: &AppState,
        tx: &mut Tx<'_>,
        decision: &Decision,
    ) -> Result<()> {
        let view =
            authority::load_locked(state, tx, &self.binding.session, Policy::IdentityOnly).await?;
        self.check_view(&view, decision)?;
        self.check_decision_link(tx, decision.decision_id).await?;
        let hot = self.load_hot(tx).await?;
        self.check_hot(&hot)?;
        fence_retained_decision(state, tx, decision, Some(self.binding.attempt_id)).await
    }
    fn check_view(&self, view: &AuthorityView, decision: &Decision) -> Result<()> {
        let selection = decision
            .outcome
            .resolved
            .as_ref()
            .ok_or_else(|| conflict("missing reserved selection"))?;
        let refused = || conflict("not the exact owned ungranted reservation");
        if !live_profiled(view) || !self.grant_matches(view) {
            return Err(refused());
        }
        if !self.binding_matches(view)
            || (view.project_id, view.task_id)
                != (selection.lineage.project_id, selection.lineage.task_id)
        {
            return Err(refused());
        }
        if view.selection_digest.as_ref()
            != Some(&digest(selection).map_err(|_| AppError::Internal)?)
        {
            return Err(refused());
        }
        Ok(())
    }
    fn grant_matches(&self, view: &AuthorityView) -> bool {
        match self.grant_id {
            None => {
                matches!(view.status, Status::Offered | Status::Preparing)
                    && view.grant_id.is_none()
            }
            Some(id) => !id.is_nil() && (view.status, view.grant_id) == (Status::Granted, Some(id)),
        }
    }
    fn binding_matches(&self, view: &AuthorityView) -> bool {
        (
            view.offer_id,
            view.execution_spec_digest.as_str(),
            view.launch_manifest_digest.as_ref(),
        ) == (
            self.binding.offer_id,
            self.binding.execution_spec_digest.as_str(),
            Some(&self.binding.launch_manifest_digest),
        )
    }
    async fn check_decision_link(&self, tx: &mut Tx<'_>, decision_id: Uuid) -> Result<()> {
        let link: Option<Uuid> = sqlx::query_scalar(
            "SELECT decision_id FROM profiled_launches WHERE attempt_id=$1 FOR SHARE",
        )
        .bind(self.binding.attempt_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?;
        if link != Some(decision_id) {
            return Err(conflict("reservation decision mismatch"));
        }
        Ok(())
    }
    async fn load_hot(&self, tx: &mut Tx<'_>) -> Result<HotState> {
        // Actual hot state is required: compatibility fixture grants cannot acquire
        // this capability by projecting grant=None from historical JSON.
        let hot: Option<Value> = sqlx::query_scalar(
            "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
        )
        .bind(self.binding.attempt_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?;
        let hot: crate::execution::authority::hot::HotState =
            serde_json::from_value(hot.ok_or_else(|| conflict("native reservation missing"))?)
                .map_err(|_| AppError::Internal)?;
        hot.validate_against(self.binding)
            .map_err(|_| conflict("invalid native reservation"))?;
        Ok(hot)
    }
    fn check_hot(&self, hot: &HotState) -> Result<()> {
        match self.grant_id {
            None if hot.grant.is_some()
                || !matches!(hot.status, Status::Offered | Status::Preparing) =>
            {
                return Err(conflict("reservation has issued authority"));
            }
            Some(id)
                if hot.status != Status::Granted
                    || hot.grant.as_ref().is_none_or(|g| g.grant_id != id) =>
            {
                return Err(conflict("initial grant replay owner changed"));
            }
            _ => {}
        }
        Ok(())
    }
}
fn live_profiled(view: &AuthorityView) -> bool {
    view.kind == Kind::ProfiledV1
        && view.holds_task
        && !view.revoked
        && !view.stopped
        && view.cancellation_id.is_none()
}

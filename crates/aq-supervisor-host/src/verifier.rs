use crate::{PlanAuthority, bridge, journal};
use aq_central_authority::{owner::*, *};
use aq_session_host::{GuardianIdentity, HostObservation, HostPhase, HostPlan};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Row, Transaction};
use std::sync::Arc;

/// F1b journaled handoff fact: the staged input re-read from the launch directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandoffFact {
    pub launch_id: uuid::Uuid,
    pub association: DispatchAssociation,
    pub disposition: HandoffDisposition,
    pub input: Digest,
}

pub(crate) struct Verifier<A> {
    pub authority: Arc<A>,
    pub candidate: Option<HostPlan>,
}
impl<A: PlanAuthority> Verifier<A> {
    async fn host(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        p: &CentralPlan,
    ) -> OwnerResult<HostPlan> {
        if let Some(h) = &self.candidate {
            return Ok(h.clone());
        }
        let body: String = sqlx::query_scalar(
            "SELECT host_plan FROM central_host_launches WHERE actor_id=$1 AND generation=$2",
        )
        .bind(p.subject.actor_id())
        .bind(p.generation as i64)
        .fetch_one(&mut **tx)
        .await?;
        HostPlan::decode(body.as_bytes()).map_err(|_| OwnerError::Evidence)
    }
}
fn evidence<T>(result: anyhow::Result<T>) -> OwnerResult<T> {
    result.map_err(|_| OwnerError::Evidence)
}
async fn original(
    tx: &mut Transaction<'_, Postgres>,
    id: uuid::Uuid,
    kind: &str,
    pin: &ArtifactRef,
) -> OwnerResult<String> {
    let body: String = sqlx::query_scalar(
        "SELECT body FROM central_host_facts WHERE launch_id=$1 AND kind=$2 AND digest=$3",
    )
    .bind(id)
    .bind(kind)
    .bind(pin.digest.as_str())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(OwnerError::Evidence)?;
    if evidence(journal::reference(kind, &body))? != *pin {
        return Err(OwnerError::Evidence);
    }
    Ok(body)
}
async fn observation(
    tx: &mut Transaction<'_, Postgres>,
    run: &Run,
    r: &HostObservationRef,
    kind: &str,
) -> OwnerResult<HostObservation> {
    let body = original(tx, r.launch_id, kind, &r.observation).await?;
    let o: HostObservation = serde_json::from_str(&body).map_err(|_| OwnerError::Evidence)?;
    let guardian = original(tx, r.launch_id, "guardian", &r.guardian).await?;
    let g: GuardianIdentity = serde_json::from_str(&guardian).map_err(|_| OwnerError::Evidence)?;
    if r.binding != run.plan().binding()?
        || o.guardian != g
        || g.intent.launch_id != r.launch_id
        || g.intent.session != run.plan().session
        || g.intent.manifest_digest != run.plan().profiles.host_plan.digest
    {
        return Err(OwnerError::Evidence);
    }
    Ok(o)
}
#[async_trait::async_trait]
impl<A: PlanAuthority> EvidenceVerifier for Verifier<A> {
    async fn authorize_plan(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        p: &CentralPlan,
    ) -> OwnerResult<()> {
        let h = self.host(tx, p).await?;
        evidence(bridge::validate_join(p, &h))?;
        evidence(self.authority.authorize(tx, p, &h).await)
    }
    async fn verify_event(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        run: &Run,
        event: &Event,
    ) -> OwnerResult<()> {
        match event {
            Event::CommitStart { launch, dispatch } => {
                let row=sqlx::query("SELECT reservation,host_plan FROM central_host_launches WHERE actor_id=$1 AND generation=$2 AND launch_id=$3")
                    .bind(run.plan().subject.actor_id()).bind(run.plan().generation as i64).bind(launch.launch_id).fetch_one(&mut **tx).await?;
                let reservation: String = row.try_get("reservation")?;
                let body: String = row.try_get("host_plan")?;
                let h = evidence(HostPlan::decode(body.as_bytes()))?;
                if *launch
                    != evidence(journal::launch_ref(
                        run.plan(),
                        launch.launch_id,
                        &reservation,
                    ))?
                    || *dispatch != run.plan().dispatch(dispatch.permit.clone())?
                {
                    return Err(OwnerError::Evidence);
                }
                evidence(bridge::permit(run.plan(), &h, &dispatch.permit))?;
            }
            Event::Running { observation: r } => {
                let o = observation(tx, run, r, "running").await?;
                if o.phase != HostPhase::Running {
                    return Err(OwnerError::Evidence);
                }
            }
            Event::Stopped { evidence: r } => {
                let o = observation(tx, run, &r.host, "stopped").await?;
                if o.phase != HostPhase::Stopped
                    || !o.kernel_wait_echild
                    || r.stopped_writers != r.host.observation
                {
                    return Err(OwnerError::Evidence);
                }
            }
            Event::HostLost {
                binding,
                launch_id,
                report,
            } => {
                original(tx, *launch_id, "lost", report).await?;
                if *binding != run.plan().binding()? {
                    return Err(OwnerError::Evidence);
                }
            }
            Event::Tick | Event::Cancel { .. } => {}
            Event::Handoff { evidence: e } | Event::ReconcileHandoff { evidence: e } => {
                let launch: uuid::Uuid = sqlx::query_scalar(
                    "SELECT launch_id FROM central_host_launches WHERE actor_id=$1 AND generation=$2",
                )
                .bind(run.plan().subject.actor_id())
                .bind(run.plan().generation as i64)
                .fetch_one(&mut **tx)
                .await?;
                let body = original(tx, launch, "handoff", &e.receipt).await?;
                let fact: HandoffFact =
                    serde_json::from_str(&body).map_err(|_| OwnerError::Evidence)?;
                let h = self.host(tx, run.plan()).await?;
                if fact.launch_id != launch
                    || fact.association != e.association
                    || fact.disposition != e.disposition
                    || run.association() != Some(&e.association)
                    || (fact.disposition == HandoffDisposition::HandedOff
                        && fact.input != h.startup.payload.digest)
                {
                    return Err(OwnerError::Evidence);
                }
            }
            Event::ReportUnknown { association } => {
                if run.association() != Some(association) {
                    return Err(OwnerError::Evidence);
                }
            }
            // Any other event has no verified evidence path; no permissive fallback.
            #[allow(unreachable_patterns)]
            _ => return Err(OwnerError::Unsupported),
        }
        Ok(())
    }
}

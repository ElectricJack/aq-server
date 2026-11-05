use anyhow::{Context, Result, ensure};
use aq_central_authority::{ArtifactRef, CentralPlan, Digest, HostLaunchRef, Text};
use aq_session_host::{GuardianIdentity, HostObservation, LaunchAuthority, LaunchIntent};
use serde::{Serialize, de::DeserializeOwned};
use sqlx::{PgPool, Row};
use std::path::Path;
use uuid::Uuid;

pub(crate) fn bytes<T: Serialize>(value: &T) -> Result<String> {
    let mut value = serde_json::to_value(value)?;
    value.sort_all_objects();
    let body = serde_json::to_string(&value)?;
    ensure!(body.len() <= 65536, "host evidence bound");
    Ok(body)
}
pub(crate) fn reference(kind: &str, body: &str) -> Result<ArtifactRef> {
    Ok(ArtifactRef {
        id: Text::new(format!("aq.central-host/{kind}"))?,
        version: Text::new("1")?,
        digest: Digest::of(body.as_bytes()),
    })
}
pub(crate) async fn append<T: Serialize>(
    pool: &PgPool,
    id: Uuid,
    kind: &str,
    value: &T,
) -> Result<ArtifactRef> {
    let body = bytes(value)?;
    let pin = reference(kind, &body)?;
    sqlx::query("INSERT INTO central_host_facts(launch_id,kind,digest,body) VALUES($1,$2,$3,$4) ON CONFLICT(launch_id,kind,digest) DO NOTHING")
        .bind(id).bind(kind).bind(pin.digest.as_str()).bind(&body).execute(pool).await?;
    Ok(pin)
}
pub(crate) async fn fact<T: DeserializeOwned>(
    pool: &PgPool,
    id: Uuid,
    kind: &str,
) -> Result<Option<T>> {
    let rows = sqlx::query(
        "SELECT body,digest FROM central_host_facts WHERE launch_id=$1 AND kind=$2 LIMIT 2",
    )
    .bind(id)
    .bind(kind)
    .fetch_all(pool)
    .await?;
    ensure!(rows.len() <= 1, "ambiguous physical fact");
    rows.first()
        .map(|row| -> Result<T> {
            let body: String = row.try_get("body")?;
            let hash: String = row.try_get("digest")?;
            ensure!(
                Digest::of(body.as_bytes()).as_str() == hash,
                "corrupt host fact"
            );
            Ok(serde_json::from_str(&body)?)
        })
        .transpose()
}
pub(crate) fn reservation(
    c: &CentralPlan,
    h: &aq_session_host::HostPlan,
    id: Uuid,
    root: &Path,
) -> Result<String> {
    bytes(
        &serde_json::json!({"binding":c.binding()?,"host_plan":h.digest()?,"launch_id":id,"root":root}),
    )
}
pub(crate) fn launch_ref(c: &CentralPlan, id: Uuid, reservation: &str) -> Result<HostLaunchRef> {
    Ok(HostLaunchRef {
        binding: c.binding()?,
        launch_id: id,
        intent: reference("reservation", reservation)?,
    })
}
pub(crate) struct Callbacks {
    pub authority: std::sync::Arc<dyn crate::PlanAuthority>,
    pub pool: PgPool,
    pub runtime: tokio::runtime::Handle,
    pub launch: Uuid,
    pub plan: aq_session_host::HostPlan,
}
impl Callbacks {
    fn check(&self, intent: &LaunchIntent) -> Result<()> {
        ensure!(
            intent.launch_id == self.launch
                && intent.session == self.plan.session
                && intent.manifest_digest == self.plan.digest()?,
            "launch callback binding"
        );
        Ok(())
    }
    fn retain<T: Serialize>(&self, kind: &str, value: &T) -> Result<()> {
        self.runtime
            .block_on(append(&self.pool, self.launch, kind, value))?;
        Ok(())
    }
}
impl LaunchAuthority for Callbacks {
    fn record_launch_intent(&mut self, i: &LaunchIntent) -> Result<()> {
        self.check(i)?;
        self.retain("intent", i)
    }
    fn record_guardian(&mut self, g: &GuardianIdentity) -> Result<()> {
        self.check(&g.intent)?;
        self.retain("guardian", g)
    }
    fn record_start_intent(&mut self, i: &LaunchIntent, g: &GuardianIdentity) -> Result<()> {
        self.check(i)?;
        ensure!(&g.intent == i, "start guardian binding");
        // The durable central permit remains authoritative even across callback delays.
        self.runtime.block_on(async {
            let mut tx=self.pool.begin().await?;
            let row=sqlx::query("SELECT s.state_bytes FROM central_session_generations s JOIN central_host_launches h USING(actor_id,generation) WHERE h.launch_id=$1")
                .bind(self.launch).fetch_one(&mut *tx).await?;
            let body:String=row.try_get("state_bytes")?;
            let aq_central_authority::Document::State(run)=aq_central_authority::decode(body.as_bytes())? else {anyhow::bail!("invalid central state")};
            ensure!(matches!(run.phase(),aq_central_authority::Phase::StartCommitted|aq_central_authority::Phase::RunningReported),"central start authority revoked");
            crate::bridge::permit(run.plan(),&self.plan,run.permit().context("missing permit")?)?;
            self.authority.authorize(&mut tx,run.plan(),&self.plan).await?;
            tx.rollback().await?;
            Ok::<_,anyhow::Error>(())
        })?;
        self.retain("start", g)
    }
    fn record_started(&mut self, o: &HostObservation) -> Result<()> {
        self.check(&o.guardian.intent)?;
        self.retain("running", o)
    }
}

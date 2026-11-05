use anyhow::{Result, ensure};
use aq_central_authority::{CentralPlan, CentralSubject, Clock, PermitData};
use aq_session_host::{ExecutionPermit, HostPlan, HostSubject};
use aq_session_protocol::Validate;

/// Read the actual host boot and suspend-aware monotonic clock, never wall time.
pub fn clock() -> Result<Clock> {
    Ok(Clock {
        machine_boot_id: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .parse()?,
        boot_ms: aq_session_host::ownership::raw_boot_ms(),
    })
}
pub fn validate_join(c: &CentralPlan, h: &HostPlan) -> Result<()> {
    c.validate()?;
    h.validate()?;
    let CentralSubject::CentralConversation {
        actor_id,
        conversation_id,
        conversation_revision,
        policy,
    } = &c.subject;
    ensure!(
        h.subject
            == HostSubject::CentralConversation {
                actor_id: *actor_id,
                conversation_id: *conversation_id,
                conversation_revision: *conversation_revision
            },
        "central host subject mismatch"
    );
    ensure!(
        c.session == h.session
            && policy == &h.policy
            && c.profiles.host_plan.digest == h.digest()?
            && c.profiles.execution == h.execution
            && c.profiles.adapter == h.harness.adapter
            && c.profiles.environment == h.harness.environment
            && c.profiles.startup == h.startup.descriptor
            && c.dispatch_id == h.input.dispatch_id
            && c.host_operation_id == h.input.host_operation_id
            && c.batch.digest()? == h.input.batch.digest
            && c.wall_time_ms == h.resources.wall_time_ms,
        "central host immutable input mismatch"
    );
    Ok(())
}
pub(crate) fn permit(c: &CentralPlan, h: &HostPlan, p: &PermitData) -> Result<ExecutionPermit> {
    validate_join(c, h)?;
    p.validate()?;
    ensure!(
        p.binding == c.binding()? && p.machine_boot_id == h.machine_boot_id,
        "central permit binding mismatch"
    );
    let result = ExecutionPermit {
        session: p.binding.session.clone(),
        manifest_digest: h.digest()?,
        sequence: p.sequence,
        issued_boot_ms: p.issued_boot_ms,
        expires_boot_ms: p.expires_boot_ms,
    };
    let now = clock()?;
    h.validate_permit_at(&result, now.machine_boot_id, now.boot_ms)?;
    Ok(result)
}

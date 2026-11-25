//! Bind each human mutation to its terminal authority and durable ledger reply.
use crate::{
    Error, Ledger, Plan,
    api::{Mutation, MutationReply},
};
use aq_session_api as api;
use aq_session_protocol::TerminalIdentity;
use uuid::Uuid;

type Result<T> = std::result::Result<T, Error>;
#[derive(Clone, Copy)]
pub enum Right {
    Inspect,
    Control,
    Both,
}
impl Mutation {
    pub fn operation_binding(&self) -> (&TerminalIdentity, Uuid, &'static str, Right, bool) {
        match self {
            Self::Watch(r) => (&r.terminal, r.operation_id, "watch", Right::Inspect, false),
            Self::Control(r) => (&r.terminal, r.operation_id, "control", Right::Control, true),
            Self::Renew(r) => (&r.terminal, r.operation_id, "renew", Right::Control, true),
            Self::Release(r) => (&r.terminal, r.operation_id, "release", Right::Control, true),
            Self::Input(r) => (&r.terminal, r.operation_id, "input", Right::Control, true),
            Self::EpochInput(r) => (
                &r.input.terminal,
                r.input.operation_id,
                "input",
                Right::Control,
                true,
            ),
            Self::OpenEpoch(r) => (
                &r.terminal,
                r.operation_id,
                "open_epoch",
                Right::Control,
                true,
            ),
            Self::Resize(r) => (&r.terminal, r.operation_id, "resize", Right::Control, true),
            Self::Capture(r) => (&r.terminal, r.operation_id, "capture", Right::Both, true),
            Self::Resolve(r) => (&r.terminal, r.operation_id, "resolve", Right::Both, true),
        }
    }
    pub fn admit(
        &self,
        ledger: &Ledger,
        admission: Admission<'_>,
        new_lease_id: impl FnOnce() -> Uuid,
    ) -> Result<(Plan, MutationReply)> {
        let Admission {
            access,
            token,
            clock,
        } = admission;
        let plan = match self {
            Self::Watch(r) => ledger.watch(access, r, clock),
            Self::Control(r) => ledger.acquire(access, r, token, new_lease_id(), clock),
            Self::Renew(r) => ledger.renew(access, r, token, new_lease_id(), clock),
            Self::Release(r) => ledger.release(access, r, token, clock),
            Self::Input(r) => ledger.input(access, r, clock),
            Self::EpochInput(r) => ledger.epoch_input(access, r, clock),
            Self::OpenEpoch(r) => ledger.open_epoch_request(access, r, token, clock),
            Self::Resize(r) => ledger.resize(access, r, clock),
            Self::Capture(r) => ledger.capture(access, r, token, clock),
            Self::Resolve(r) => ledger.resolve(access, r, clock),
        }?;
        let reply = admission.reply(self, plan.preview())?;
        Ok((plan, reply))
    }
}
pub struct Admission<'a> {
    pub access: &'a crate::Access,
    pub token: Uuid,
    pub clock: u64,
}
impl Admission<'_> {
    fn reply(&self, request: &Mutation, n: crate::LedgerSummary) -> Result<MutationReply> {
        let (t, id, _, _, _) = request.operation_binding();
        let reply = match request {
            Mutation::Watch(r) => MutationReply::Watch {
                terminal: t.clone(),
                operation_id: id,
                subscription_id: id,
                session_revision: n.session_revision,
                expires_at_ms: r.expires_at_ms,
            },
            Mutation::Control(_) | Mutation::Renew(_) => MutationReply::Control {
                operation_id: id,
                status: n.control,
            },
            Mutation::Release(_) => MutationReply::Released {
                terminal: t.clone(),
                operation_id: id,
                generation: n.generation,
            },
            Mutation::Input(_) | Mutation::EpochInput(_) => {
                MutationReply::Input(api::InputReceipt {
                    operation_id: id,
                    terminal: t.clone(),
                    disposition: aq_session_protocol::InputDisposition::Queued,
                    input_paused: n.control.input_paused,
                    observed_at_ms: self.clock,
                })
            }
            Mutation::OpenEpoch(_) => MutationReply::Epoch {
                operation_id: id,
                permit: n.epoch_retirement.ok_or(Error::Persistence)?.permit,
            },
            Mutation::Resize(r) => MutationReply::Resize {
                terminal: t.clone(),
                operation_id: id,
                sequence: r.sequence,
            },
            Mutation::Capture(r) => MutationReply::Capture {
                terminal: t.clone(),
                operation_id: id,
                capture_request_id: self.token,
                expires_at_ms: r.expires_at_ms.min(self.access.valid_until_ms).min(
                    n.control
                        .lease
                        .as_ref()
                        .ok_or(Error::Persistence)?
                        .expires_at_ms,
                ),
            },
            Mutation::Resolve(r) => MutationReply::Input(api::InputReceipt {
                operation_id: r.uncertain_operation_id,
                terminal: t.clone(),
                disposition: aq_session_protocol::InputDisposition::DeliveryUnknown,
                input_paused: n.control.input_paused,
                observed_at_ms: self.clock,
            }),
        };
        Ok(reply)
    }
}

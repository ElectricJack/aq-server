//! The D1 automatic publish for D3's default completion policy, compiled out
//! with the cargo feature `delivery_playbook` (Epic 0, spec 0016 section 7).
use super::{SourceHead, current_head};
use crate::{
    AppState,
    delivery::{DeliverRequest, DeliveryError, DeliveryService, PublicationMode},
    profiled_execution::{
        results,
        settlement::{
            automatic::{AutoDisposition, AutoSettlement, settle_after_record},
            owner::*,
        },
    },
};
use aq_worker_protocol::{ObjectAlgorithm, ObjectId};
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Mutex};
use uuid::Uuid;

const HELD: &str = "delivery_evidence_missing";

// aq-git 0.2.0 cannot create a pull-request head branch and no GitHub
// credentials are configured (D0/D2): pull-request policies hold explicitly
// before any call, so no GitHub client is bound.
use crate::delivery::UnconfiguredGitHub as NoGitHub;

/// One in-process owner per report: a duplicate result POST never races a
/// running publication into a second evidence write.
static IN_FLIGHT: Mutex<Option<HashSet<Uuid>>> = Mutex::new(None);
struct Flight(Uuid);
impl Flight {
    fn take(report: Uuid) -> Option<Self> {
        let mut set = IN_FLIGHT.lock().ok()?;
        set.get_or_insert_with(HashSet::new)
            .insert(report)
            .then_some(Self(report))
    }
}
impl Drop for Flight {
    fn drop(&mut self) {
        if let Ok(mut set) = IN_FLIGHT.lock()
            && let Some(set) = set.as_mut()
        {
            set.remove(&self.0);
        }
    }
}

/// True when D3 holds this settlement only on missing delivery evidence.
pub fn awaits_delivery(s: &AutoSettlement) -> bool {
    s.disposition == AutoDisposition::Held && s.reason.as_deref() == Some(HELD)
}

/// Publish a held result and let D3 re-evaluate it. Idempotent per report.
/// The caller runs this detached from any client request, so a dropped
/// request can never cancel a publication between dispatch and receipt.
pub async fn deliver_then_settle(
    state: &AppState,
    delivery: &DeliveryService,
    report_id: Uuid,
) -> Result<Option<AutoSettlement>> {
    let Some(_flight) = Flight::take(report_id) else {
        return Ok(None);
    };
    let Some(settled) = settle_after_record(state, report_id).await? else {
        return Ok(None);
    };
    if !awaits_delivery(&settled) {
        return Ok(Some(settled));
    }
    match publish(state, delivery, report_id).await? {
        Step::Held => Ok(Some(settled)),
        // Evidence was recorded, or acceptance no longer holds (for example a
        // cancellation landed): D3 re-evaluates either way.
        Step::Recorded | Step::NotAccepted => settle_after_record(state, report_id).await,
    }
}

enum Step {
    Recorded,
    /// Nothing recorded; the result stays held for a later replay.
    Held,
    /// D3's acceptance no longer holds; nothing was sent to aq-git's publish.
    NotAccepted,
}

mod admission;
mod candidate;
mod dispatch;
mod evidence;
use crate::delivery::{
    publication_evidence::{
        Candidate, PublicationStatus, PublicationTarget, Recorded, SavedCheckpoint,
    },
    source_checks::Repository,
};
use admission::{Fence, fence, release_fence};
use candidate::Publication;
use evidence::Prepared;

/// Publish only the retained source selected for this result.
async fn publish(state: &AppState, delivery: &DeliveryService, report_id: Uuid) -> Result<Step> {
    let Some(publication) = Publication::load(state, report_id).await? else {
        return Ok(Step::Held);
    };
    let Some(prepared) = publication.prepare(state, delivery).await? else {
        return Ok(Step::Held);
    };
    prepared.publish(state, delivery).await
}

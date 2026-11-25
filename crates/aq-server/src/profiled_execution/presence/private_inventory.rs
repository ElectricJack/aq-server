//! Private provisioning reads cannot occupy Tokio's shared blocking queue.
use super::unavailable;
use crate::{error::AppError, intelligence::config::LoadedRegistry};
use aq_selection::{BackendKind, CredentialBinding, DeliveryCapability};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
use uuid::Uuid;
struct Job {
    registry: Arc<LoadedRegistry>,
    node: Uuid,
    boot: Uuid,
    delivery: DeliveryCapability,
    observed: u64,
    expires: u64,
    backend_kind: Option<BackendKind>,
    reply: oneshot::Sender<Vec<CredentialBinding>>,
    _permit: OwnedSemaphorePermit,
}
struct Readers {
    send: mpsc::SyncSender<Job>,
    permits: Arc<Semaphore>,
}
impl Readers {
    fn start() -> Result<Self, ()> {
        let (send, receive) = mpsc::sync_channel::<Job>(2);
        let receive = Arc::new(Mutex::new(receive));
        for i in 0..2 {
            let receive = receive.clone();
            std::thread::Builder::new()
                .name(format!("aq-presence-provision-{i}"))
                .spawn(move || {
                    loop {
                        let job = match receive.lock() {
                            Ok(r) => match r.recv() {
                                Ok(j) => j,
                                Err(_) => return,
                            },
                            Err(_) => return,
                        };
                        let result = crate::profiled_execution::material::provisioning_bindings(
                            &job.registry,
                            aq_intelligence_config::material::ProvisioningSnapshot {
                                node_id: job.node,
                                boot_id: job.boot,
                                delivery: &job.delivery,
                                observed_at_ms: job.observed,
                                expires_at_ms: job.expires,
                                backend_kind: job.backend_kind,
                            },
                        );
                        let _ = job.reply.send(result);
                    }
                })
                .map_err(|_| ())?;
        }
        Ok(Self {
            send,
            permits: Arc::new(Semaphore::new(4)),
        })
    }
}
pub(super) async fn observe(
    registry: Arc<LoadedRegistry>,
    node: Uuid,
    boot: Uuid,
    delivery: DeliveryCapability,
    observed: u64,
    expires: u64,
    backend_kind: Option<BackendKind>,
) -> Result<Vec<CredentialBinding>, AppError> {
    static READERS: OnceLock<Result<Readers, ()>> = OnceLock::new();
    let readers = READERS
        .get_or_init(Readers::start)
        .as_ref()
        .map_err(|_| unavailable())?;
    let permit = readers
        .permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| unavailable())?;
    let (reply, receive) = oneshot::channel();
    readers
        .send
        .try_send(Job {
            registry,
            node,
            boot,
            delivery,
            observed,
            expires,
            backend_kind,
            reply,
            _permit: permit,
        })
        .map_err(|_| unavailable())?;
    tokio::time::timeout(std::time::Duration::from_secs(2), receive)
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())
}

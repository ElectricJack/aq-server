//! Reserved private read workers. A cancelled waiter never frees a job's permit
//! before its actual read finishes; no raw material leaves the worker thread.
use crate::{error::AppError, intelligence::config::LoadedRegistry, profiled_execution::material};
use aq_admission::{Digest, MaterialLease};
use std::sync::{
    Arc, Mutex, OnceLock,
    mpsc::{self, SyncSender},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
use uuid::Uuid;
#[derive(Clone)]
pub struct Fingerprint {
    pub digest: Digest,
    pub bytes: u64,
}
struct Input {
    registry: Arc<LoadedRegistry>,
    lease: MaterialLease,
    node: Uuid,
    source: Option<crate::profiled_execution::login_refresh::HeadSource>,
    #[cfg(test)]
    gate: Option<Arc<tests::Gate>>,
}
struct Job {
    input: Input,
    reply: oneshot::Sender<Result<Fingerprint, material::MaterialError>>,
    _ownership: OwnedSemaphorePermit,
}
struct Readers {
    send: SyncSender<Job>,
    permits: Arc<Semaphore>,
}
impl Readers {
    fn start() -> Result<Self, ()> {
        Self::with_reader(read)
    }
    fn start_phase() -> Result<Self, ()> {
        Self::with_reader_named(read, "aq-phase-material")
    }
    fn with_reader(
        reader: fn(&Input) -> Result<Fingerprint, material::MaterialError>,
    ) -> Result<Self, ()> {
        Self::with_reader_named(reader, "aq-renew-material")
    }
    fn with_reader_named(
        reader: fn(&Input) -> Result<Fingerprint, material::MaterialError>,
        name: &str,
    ) -> Result<Self, ()> {
        let (send, receive) = mpsc::sync_channel::<Job>(2);
        let receive = Arc::new(Mutex::new(receive));
        for index in 0..2 {
            let receive = receive.clone();
            std::thread::Builder::new()
                .name(format!("{name}-{index}"))
                .spawn(move || {
                    loop {
                        let job = match receive.lock() {
                            Ok(receive) => match receive.recv() {
                                Ok(job) => job,
                                Err(_) => return,
                            },
                            Err(_) => return,
                        };
                        let result = reader(&job.input);
                        let _ = job.reply.send(result);
                        // The ownership permit drops only after read+zeroization, even
                        // if the HTTP waiter timed out or its receiving future vanished.
                    }
                })
                .map_err(|_| ())?;
        }
        Ok(Self {
            send,
            permits: Arc::new(Semaphore::new(4)),
        })
    }
    fn submit(
        &self,
        input: Input,
    ) -> Result<oneshot::Receiver<Result<Fingerprint, material::MaterialError>>, AppError> {
        let ownership = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| unavailable())?;
        let (reply, receive) = oneshot::channel();
        self.send
            .try_send(Job {
                input,
                reply,
                _ownership: ownership,
            })
            .map_err(|_| unavailable())?;
        Ok(receive)
    }
}
fn read(input: &Input) -> Result<Fingerprint, material::MaterialError> {
    let material = material::read_for_lease_from(
        &input.registry,
        &input.lease,
        input.node,
        input.source.as_ref(),
    )?;
    Ok(Fingerprint {
        digest: Digest::of(material.bytes()),
        bytes: material.bytes().len() as u64,
    })
    // ProtectedMaterial zeroizes before the digest-only result is delivered.
}
fn unavailable() -> AppError {
    AppError::UpdatesUnavailable("current private material observation unavailable".into())
}
pub async fn verify(
    registry: Arc<LoadedRegistry>,
    lease: MaterialLease,
    node: Uuid,
    source: Option<crate::profiled_execution::login_refresh::HeadSource>,
) -> Result<Fingerprint, AppError> {
    static READERS: OnceLock<Result<Readers, ()>> = OnceLock::new();
    let readers = READERS
        .get_or_init(Readers::start)
        .as_ref()
        .map_err(|_| unavailable())?;
    verify_with(readers, registry, lease, node, source).await
}
/// Separate initial-phase pool; initial Grant work never consumes renewal reserve.
pub(crate) async fn verify_phase(
    registry: Arc<LoadedRegistry>,
    lease: MaterialLease,
    node: Uuid,
    source: Option<crate::profiled_execution::login_refresh::HeadSource>,
) -> Result<Fingerprint, AppError> {
    static READERS: OnceLock<Result<Readers, ()>> = OnceLock::new();
    let readers = READERS
        .get_or_init(Readers::start_phase)
        .as_ref()
        .map_err(|_| unavailable())?;
    verify_with(readers, registry, lease, node, source).await
}
async fn verify_with(
    readers: &Readers,
    registry: Arc<LoadedRegistry>,
    lease: MaterialLease,
    node: Uuid,
    source: Option<crate::profiled_execution::login_refresh::HeadSource>,
) -> Result<Fingerprint, AppError> {
    let reply = readers.submit(Input {
        registry,
        lease,
        node,
        source,
        #[cfg(test)]
        gate: None,
    })?;
    tokio::time::timeout(std::time::Duration::from_secs(2), reply)
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?
        .map_err(|error| match error {
            material::MaterialError::Unavailable => unavailable(),
            material::MaterialError::Changed | material::MaterialError::Scope => {
                AppError::Conflict(
                    "installed private material binding changed or is unauthorized".into(),
                )
            }
        })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::intelligence::config::{CredentialConfig, IntelligenceConfig, MaterialDeliveryMode};
    use aq_admission::{ArtifactRef, Label, LaunchBinding, SessionIdentity, Text};
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{
            Condvar,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };
    fn artifact(id: &str) -> ArtifactRef {
        ArtifactRef {
            id: Text::new(id).unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(id.as_bytes()),
        }
    }
    fn input() -> (tempfile::TempDir, Input) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private");
        std::fs::write(&path, b"synthetic-private-test-material").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let node = Uuid::new_v4();
        let attempt = Uuid::new_v4();
        let profile = artifact("test-account");
        let registry = IntelligenceConfig {
            accounting: Default::default(),
            material_store: None,
            revision: 1,
            credentials: vec![CredentialConfig {
                kind: Default::default(),
                allowed_backends: aq_selection::default_allowed_backends(),
                credential: aq_selection::AuthorizedCredential {
                    profile: profile.clone(),
                    provider: "fixture".into(),
                    account_reference: "account".into(),
                    endpoint_reference: "endpoint".into(),
                    revision: 1,
                    enabled: true,
                    provisionable: false,
                    material_owner: None,
                    allowed_backends: aq_selection::default_allowed_backends(),
                },
                allowed_node_ids: vec![node],
                secret_file: Some(path),
                delivery_mode: Some(MaterialDeliveryMode::ServerMaterialV1),
            }],
            qualified_harnesses: vec![],
        }
        .load()
        .unwrap();
        let binding = LaunchBinding {
            schema_version: 1,
            session: SessionIdentity::Remote {
                node_id: node,
                boot_id: Uuid::new_v4(),
                attempt_id: attempt,
                assignment_generation: 1,
                session_id: Uuid::new_v4(),
            },
            offer_id: Uuid::new_v4(),
            attempt_id: attempt,
            assignment_generation: 1,
            execution_spec_digest: Digest::of(b"spec"),
            launch_manifest_digest: Digest::of(b"manifest"),
            graph_access_profile_digest: Digest::of(b"profile"),
        };
        let lease = MaterialLease {
            schema_version: 1,
            lease_id: Uuid::new_v4(),
            binding,
            credential_profile: profile,
            credential_revision: 1,
            provider: Label::new("fixture").unwrap(),
            account_reference: Label::new("account").unwrap(),
            endpoint_reference: Label::new("endpoint").unwrap(),
            purpose: Label::new("model-authentication").unwrap(),
            material_digest: Digest::of(b"synthetic-private-test-material"),
            material_bytes: b"synthetic-private-test-material".len() as u64,
            issued_at_ms: 1,
            expires_at_ms: 2,
        };
        // Length is real, not a token/window estimate or zero placeholder.
        (
            dir,
            Input {
                registry: Arc::new(registry),
                lease,
                node,
                source: None,
                gate: None,
            },
        )
    }
    pub struct Gate {
        open: Mutex<bool>,
        changed: Condvar,
        active: AtomicUsize,
        maximum: AtomicUsize,
    }
    impl Gate {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                open: Mutex::new(false),
                changed: Condvar::new(),
                active: AtomicUsize::new(0),
                maximum: AtomicUsize::new(0),
            })
        }
        fn release(&self) {
            *self.open.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }
    struct Release(Arc<Gate>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    fn blocked(input: &Input) -> Result<Fingerprint, material::MaterialError> {
        let gate = input.gate.as_ref().unwrap();
        let active = gate.active.fetch_add(1, Ordering::SeqCst) + 1;
        gate.maximum.fetch_max(active, Ordering::SeqCst);
        let mut open = gate.open.lock().unwrap();
        while !*open {
            open = gate.changed.wait(open).unwrap();
        }
        gate.active.fetch_sub(1, Ordering::SeqCst);
        read(input)
    }
    #[tokio::test]
    async fn cancelled_waiters_keep_actual_jobs_bounded_until_read_returns() {
        let readers = Readers::with_reader(blocked).unwrap();
        let gate = Gate::new();
        let _release = Release(gate.clone());
        let mut dirs = vec![];
        for _ in 0..2 {
            let (dir, mut input) = input();
            input.gate = Some(gate.clone());
            dirs.push(dir);
            drop(readers.submit(input).unwrap());
        }
        let start = Instant::now();
        while gate.active.load(Ordering::SeqCst) < 2 {
            assert!(start.elapsed() < Duration::from_secs(2));
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        for _ in 0..2 {
            let (dir, mut input) = input();
            input.gate = Some(gate.clone());
            dirs.push(dir);
            drop(readers.submit(input).unwrap());
        }
        assert_eq!(readers.permits.available_permits(), 0);
        let (_dir, extra) = input();
        assert!(readers.submit(extra).is_err());
        gate.release();
        let start = Instant::now();
        while readers.permits.available_permits() != 4 {
            assert!(start.elapsed() < Duration::from_secs(2));
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(gate.maximum.load(Ordering::SeqCst), 2);
    }
    #[test]
    fn real_private_read_ignores_saturated_tokio_blocking_pool_and_old_retrieval_expiry() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let gate = Gate::new();
            let release = Release(gate.clone());
            let g = gate.clone();
            let held = tokio::task::spawn_blocking(move || {
                let mut open = g.open.lock().unwrap();
                g.active.store(1, Ordering::SeqCst);
                while !*open {
                    open = g.changed.wait(open).unwrap();
                }
            });
            let start = Instant::now();
            while gate.active.load(Ordering::SeqCst) == 0 {
                assert!(start.elapsed() < Duration::from_secs(2));
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            let (_dir, input) = input();
            let lease = input.lease.clone();
            let fingerprint = tokio::time::timeout(
                Duration::from_millis(500),
                verify(input.registry, lease, input.node, None),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(
                fingerprint.digest,
                Digest::of(b"synthetic-private-test-material")
            );
            assert_eq!(
                fingerprint.bytes,
                b"synthetic-private-test-material".len() as u64
            );
            drop(release);
            held.await.unwrap();
        });
    }
}

//! Parent-side child bookkeeping only. It never signals, renews or proves stop.
use super::*;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicUsize, Ordering},
};
pub const MAX_LOCAL_GUARDIANS: usize = 1024;
struct Registry {
    children: Mutex<Vec<Child>>,
    slots: AtomicUsize,
}
static REGISTRY: OnceLock<std::result::Result<Arc<Registry>, String>> = OnceLock::new();
fn registry() -> Result<Arc<Registry>> {
    REGISTRY
        .get_or_init(|| {
            let registry = Arc::new(Registry {
                children: Mutex::new(Vec::with_capacity(MAX_LOCAL_GUARDIANS)),
                slots: AtomicUsize::new(0),
            });
            let worker = registry.clone();
            std::thread::Builder::new()
                .name("aq-host-reaper".into())
                .stack_size(128 * 1024)
                .spawn(move || {
                    loop {
                        {
                            let mut children =
                                worker.children.lock().unwrap_or_else(|p| p.into_inner());
                            let mut i = 0;
                            while i < children.len() {
                                let done = match children[i].try_wait() {
                                    Ok(Some(_)) => true,
                                    Ok(None) => false,
                                    Err(e) => e.raw_os_error() == Some(libc::ECHILD),
                                };
                                if done {
                                    let mut child = children.swap_remove(i);
                                    let _ = child.try_wait();
                                    worker.slots.fetch_sub(1, Ordering::AcqRel);
                                } else {
                                    i += 1
                                }
                            }
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                })
                .map(|_| registry)
                .map_err(|e| format!("parent reaper unavailable: {e}"))
        })
        .as_ref()
        .map(Arc::clone)
        .map_err(|e| anyhow::anyhow!(e.clone()))
}
/// A slot is reserved before launch intent/callback effects, and cannot be reused
/// until its tracked helper has been reaped. Exhaustion refuses before spawn.
pub(crate) struct Slot {
    registry: Arc<Registry>,
    transferred: bool,
}
impl Slot {
    pub(crate) fn reserve() -> Result<Self> {
        Self::reserve_from(registry()?)
    }
    fn reserve_from(registry: Arc<Registry>) -> Result<Self> {
        // fetch_update renamed try_update on newer stable; keep until toolchain pin.
        #[allow(deprecated)]
        let reserved = registry
            .slots
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_LOCAL_GUARDIANS).then_some(n + 1)
            });
        ensure!(reserved.is_ok(), "local guardian reaper capacity exhausted");
        Ok(Self {
            registry,
            transferred: false,
        })
    }
    pub(crate) fn track(mut self, child: Child) -> Result<std::os::fd::OwnedFd> {
        let pid = child.id();
        // Open while this Child is still exclusively ours: publishing it first
        // lets the registry reap an exited helper and the PID be reused before
        // pidfd_open. Even an open failure must transfer cleanup ownership.
        let fd = ownership::pidfd(pid);
        self.registry
            .children
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(child);
        self.transferred = true;
        fd
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        if !self.transferred {
            self.registry.slots.fetch_sub(1, Ordering::AcqRel);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reservation_exhaustion_refuses_before_spawning() {
        let registry = Arc::new(Registry {
            children: Mutex::new(Vec::with_capacity(MAX_LOCAL_GUARDIANS)),
            slots: AtomicUsize::new(0),
        });
        let mut held: Vec<_> = (0..MAX_LOCAL_GUARDIANS)
            .map(|_| Slot::reserve_from(registry.clone()).unwrap())
            .collect();
        assert!(Slot::reserve_from(registry.clone()).is_err());
        drop(held.pop());
        let replacement = Slot::reserve_from(registry.clone()).unwrap();
        assert_eq!(registry.slots.load(Ordering::Acquire), MAX_LOCAL_GUARDIANS);
        drop(replacement);
        drop(held);
        assert_eq!(registry.slots.load(Ordering::Acquire), 0);
    }
}

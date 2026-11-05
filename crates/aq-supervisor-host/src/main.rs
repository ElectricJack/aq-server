use anyhow::{Context, Result, ensure};
use aq_session_host::{LocalEnvironment, ResourcePolicy};
use aq_supervisor_host::{
    RETRY_ATTEMPTS, RETRY_BACKOFF, Supervisor,
    adapter::NativeAdapter,
    config::{Config, InstalledAuthority},
    is_transient, retry_bounded,
};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::signal::unix::{Signal, SignalKind, signal};

struct ShutdownSignals {
    interrupt: Signal,
    terminate: Signal,
}

impl ShutdownSignals {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    async fn recv(&mut self) -> Result<()> {
        tokio::select! {
            received = self.interrupt.recv() => received,
            received = self.terminate.recv() => received,
        }
        .context("shutdown signal stream closed")
    }
}

#[tokio::main(worker_threads = 2)]
async fn main() {
    // Do not print nested IO/config/provider errors: they can contain placement.
    if run().await.is_err() {
        eprintln!("supervisor host refused or failed; retained state requires inspection");
        std::process::exit(1);
    }
}
async fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() == 4 && args[1] == "native" {
        return aq_supervisor_host::adapter::native(std::path::Path::new(&args[2]), &args[3]).await;
    }
    ensure!(
        args.len() == 3 && matches!(args[1].as_str(), "run" | "stop" | "status"),
        "usage: aq-supervisor-host run|stop|status CONFIG"
    );
    ensure!(
        unsafe { libc::geteuid() } == 0,
        "host requires separated root and native identities"
    );
    let path = PathBuf::from(&args[2]);
    let (c, digest) = Config::load(&path)?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&c.database_url)
        .await?;
    let authority = Arc::new(InstalledAuthority {
        path: path.clone(),
        digest: digest.clone(),
    });
    let adapter = Arc::new(NativeAdapter {
        config: path,
        digest,
        wrapper: std::env::current_exe()?,
    });
    let local = LocalEnvironment {
        workspace: c.workspace.clone(),
        helper_executable: c.helper,
        tmux_executable: c.tmux,
        tmux_version: c.tmux_version,
        resource_policy: ResourcePolicy::CallerAdmission(c.resource_admission),
        credential_profiles: vec![],
        context_material: vec![],
    };
    let host = Supervisor::new(pool, c.root, local, authority, adapter);
    let actor = c.central.subject.actor_id();
    match args[1].as_str() {
        "status" => {
            let s = host.inspect(actor).await?;
            println!(
                "generation={} phase={:?} revision={}",
                s.run.plan().generation,
                s.run.phase(),
                s.revision
            );
        }
        "stop" => {
            host.stop(actor).await?;
        }
        "run" => {
            // Register before launch so a stop during startup remains pending.
            let mut signals = ShutdownSignals::new()?;
            let ttl = c.central.max_permit_ms.min(120_000);
            let startup = aq_supervisor_host::config::protected_read(&c.startup_file, 1048576)?;
            let mut s = Box::pin(host.start(c.central, c.host, startup, ttl)).await?;
            let at_ms = u64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_millis(),
            )?;
            host.enable_replies(actor, at_ms).await?;
            let shutdown = signals.recv();
            tokio::pin!(shutdown);
            loop {
                tokio::select! {result=&mut shutdown=>{result?;host.stop(actor).await?;break;},_ = tokio::time::sleep(Duration::from_millis(250))=>{}}
                // A transient DB/network error must not exit the controller while
                // the guardian keeps running; retry within the last known permit.
                let expires = s.run.permit().map_or(0, |p| p.expires_boot_ms);
                s = retry_bounded(RETRY_ATTEMPTS, RETRY_BACKOFF, expires, is_transient, || {
                    host.reconcile(actor)
                })
                .await?;
                if !matches!(
                    s.run.phase(),
                    aq_central_authority::Phase::StartCommitted
                        | aq_central_authority::Phase::RunningReported
                ) {
                    break;
                }
                let p = s.run.permit().context("permit absent")?;
                let now = aq_supervisor_host::clock()?.boot_ms;
                if now >= p.issued_boot_ms + (p.expires_boot_ms - p.issued_boot_ms) / 2
                    && p.expires_boot_ms < s.run.original_deadline_boot_ms()
                {
                    let expires = p.expires_boot_ms;
                    let sequence = p.sequence;
                    s = retry_bounded(RETRY_ATTEMPTS, RETRY_BACKOFF, expires, is_transient, || {
                        host.renew(actor, sequence, ttl)
                    })
                    .await?;
                }
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_receives_sigterm_and_sigint() {
        let mut signals = ShutdownSignals::new().unwrap();
        for signal in [libc::SIGTERM, libc::SIGINT] {
            // Deliver a real OS signal before polling, as can happen at startup.
            assert_eq!(unsafe { libc::kill(libc::getpid(), signal) }, 0);
            tokio::time::timeout(Duration::from_secs(1), signals.recv())
                .await
                .expect("shutdown signal was not received")
                .unwrap();
        }
    }
}

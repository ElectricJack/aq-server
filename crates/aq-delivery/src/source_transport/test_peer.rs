//! Disposable foreground Git service for private source-transfer qualifications.
use nix::{
    sys::signal::{Signal, kill, killpg},
    unistd::Pid,
};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::process::{Child, Command};

pub struct TestGit(Child);
impl TestGit {
    pub async fn start(root: &Path, socket: &Path, polls: usize) -> Self {
        let binary =
            std::env::var("AQ_TEST_GIT_CANDIDATE").expect("set immutable aq-git candidate");
        let git = std::env::var("AQ_TEST_GIT_EXECUTABLE").expect("set selected Git executable");
        let child = Command::new(binary)
            .arg("--state-dir")
            .arg(root)
            .arg("--git-executable")
            .arg(git)
            .arg("--listen")
            .arg(socket)
            .arg("serve")
            .process_group(0)
            .kill_on_drop(true)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut owner = Self(child);
        for _ in 0..polls {
            if socket.exists() {
                break;
            }
            assert!(owner.0.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        owner
    }

    /// SIGTERM lets the foreground guard shut down and reap the engine it owns.
    pub async fn stop(mut self) {
        let pid = Pid::from_raw(self.0.id().unwrap() as i32);
        kill(pid, Signal::SIGTERM).unwrap();
        let status = tokio::time::timeout(Duration::from_secs(10), self.0.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success());
    }
}
impl Drop for TestGit {
    fn drop(&mut self) {
        // On assertion failure, stop only the group whose leader we still own.
        // Child::id becomes None once the foreground process has been reaped.
        if let Some(pid) = self.0.id() {
            let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
    }
}

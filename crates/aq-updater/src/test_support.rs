//! Shared helpers for unit tests.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// Set while probing a stub: the guard line `write_stub` adds exits at once.
const PROBE_ENV: &str = "AQ_UPDATER_TEST_STUB_PROBE";

/// Writes an executable shell stub (for example a fake `docker`) at `path`
/// and returns once it can be executed.
///
/// Tests run in parallel threads of one process, so another test can fork
/// while this file is still open for writing; the forked child holds that
/// writable descriptor until it execs, and executing the stub in that window
/// fails with ETXTBSY ("Text file busy"). The stub is written under a temp
/// name and renamed into place (a new inode, never one being executed), then
/// probed: once one exec succeeds no descriptor for writing is left, and none
/// can reappear, so every later exec by the test succeeds.
pub(crate) fn write_stub(path: &Path, script: &str) {
    let body = script
        .strip_prefix("#!/bin/sh\n")
        .unwrap_or_else(|| panic!("stub must start with #!/bin/sh: {script:?}"));
    let tmp = path.with_extension("stub-tmp");
    std::fs::write(
        &tmp,
        format!("#!/bin/sh\n[ -z \"${PROBE_ENV}\" ] || exit 0\n{body}"),
    )
    .unwrap();
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&tmp, path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match std::process::Command::new(path)
            .env(PROBE_ENV, "1")
            .status()
        {
            Ok(s) => {
                assert!(s.success(), "stub probe failed: {s}");
                return;
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(e) => panic!("cannot execute stub {}: {e}", path.display()),
        }
    }
}

//! Release authenticity. Production uses cosign keyless verification against
//! the release workflow's identity; the ed25519 `TestKey` exists only with
//! the `test-verifier` feature.

use crate::config::VerifierConfig;
use crate::runtime::stderr_tail;
use aq_release::ReleaseManifest;
use async_trait::async_trait;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// How long `cosign verify-blob` may run before it is killed.
pub const COSIGN_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    /// The signature does not authenticate the manifest.
    #[error("release signature rejected: {0}")]
    Rejected(String),
    /// The verifier could not run (missing binary, bad key configuration).
    #[error("release verifier unavailable: {0}")]
    Unavailable(String),
}

#[async_trait]
pub trait ReleaseVerifier: Send + Sync {
    /// Check that `bundle` authenticates exactly these `manifest` bytes.
    async fn verify(&self, manifest: &[u8], bundle: &[u8]) -> Result<(), VerifyError>;
}

#[async_trait]
impl ReleaseVerifier for Box<dyn ReleaseVerifier> {
    async fn verify(&self, manifest: &[u8], bundle: &[u8]) -> Result<(), VerifyError> {
        (**self).verify(manifest, bundle).await
    }
}

/// Build the verifier the config names.
pub fn from_config(cfg: &VerifierConfig) -> Result<Box<dyn ReleaseVerifier>, VerifyError> {
    match cfg {
        VerifierConfig::CosignKeyless {
            cosign_path,
            identity_regexp,
            oidc_issuer,
        } => Ok(Box::new(CosignKeyless::new(
            cosign_path.clone(),
            identity_regexp.clone(),
            oidc_issuer.clone(),
        ))),
        #[cfg(feature = "test-verifier")]
        VerifierConfig::TestKey { public_key_hex } => {
            Ok(Box::new(TestKey::from_hex(public_key_hex)?))
        }
    }
}

/// What a keyless signature must have been made for, read from the manifest
/// it signs (still unverified at that point): `release.yml` running on the
/// tag `refs/tags/v<version>` at the commit `source_revision`. Without this,
/// any release's bundle would authenticate any manifest it was made for,
/// whatever version and revision that manifest names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseBinding {
    /// `refs/tags/v<version>`: the certificate's GitHub workflow ref.
    pub tag_ref: String,
    /// The tagged commit: the certificate's GitHub workflow SHA.
    pub sha: String,
}

/// Parse the binding out of untrusted manifest bytes. A manifest that does
/// not parse, or whose `source_revision` is not a full commit SHA, is
/// refused: there is nothing to bind the signature to.
pub fn release_binding(manifest: &[u8]) -> Result<ReleaseBinding, VerifyError> {
    let m = ReleaseManifest::from_json(manifest).map_err(|e| {
        VerifyError::Rejected(format!(
            "the manifest cannot be bound to its signature: {e}"
        ))
    })?;
    let sha = &m.source_revision;
    if sha.len() != 40 || !sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(VerifyError::Rejected(format!(
            "the manifest's source_revision {sha:.64?} is not a 40-character lowercase commit SHA, so the signature cannot be bound to it"
        )));
    }
    Ok(ReleaseBinding {
        tag_ref: format!("refs/tags/v{}", m.version),
        sha: sha.clone(),
    })
}

/// `cosign verify-blob` with a fixed argument vector; no shell.
#[derive(Debug, Clone)]
pub struct CosignKeyless {
    cosign_path: PathBuf,
    identity_regexp: String,
    oidc_issuer: String,
    timeout: Duration,
}

impl CosignKeyless {
    pub fn new(cosign_path: PathBuf, identity_regexp: String, oidc_issuer: String) -> Self {
        CosignKeyless {
            cosign_path,
            identity_regexp,
            oidc_issuer,
            timeout: COSIGN_TIMEOUT,
        }
    }

    /// The identity regexp and issuer pin the signer to `release.yml` on a
    /// `vX.Y.Z` tag; the GitHub workflow ref and SHA (cosign v3 flags, read
    /// from the certificate's Fulcio extensions) pin it to the tag and
    /// commit this manifest names.
    fn args(&self, bundle: &Path, manifest: &Path, binding: &ReleaseBinding) -> Vec<OsString> {
        vec![
            "verify-blob".into(),
            "--bundle".into(),
            bundle.into(),
            "--certificate-identity-regexp".into(),
            self.identity_regexp.clone().into(),
            "--certificate-oidc-issuer".into(),
            self.oidc_issuer.clone().into(),
            "--certificate-github-workflow-ref".into(),
            binding.tag_ref.clone().into(),
            "--certificate-github-workflow-sha".into(),
            binding.sha.clone().into(),
            manifest.into(),
        ]
    }
}

#[async_trait]
impl ReleaseVerifier for CosignKeyless {
    async fn verify(&self, manifest: &[u8], bundle: &[u8]) -> Result<(), VerifyError> {
        let unavailable =
            |what: &str, e: std::io::Error| VerifyError::Unavailable(format!("{what}: {e}"));
        let binding = release_binding(manifest)?;
        let dir =
            tempfile::TempDir::new().map_err(|e| unavailable("cannot create a temp dir", e))?;
        let m = dir.path().join("manifest.json");
        let b = dir.path().join("manifest.sigstore.json");
        std::fs::write(&m, manifest).map_err(|e| unavailable("cannot write the manifest", e))?;
        std::fs::write(&b, bundle).map_err(|e| unavailable("cannot write the bundle", e))?;
        // kill_on_drop: when the timeout drops the wait, the child is killed.
        let child = tokio::process::Command::new(&self.cosign_path)
            .args(self.args(&b, &m, &binding))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| unavailable(&format!("cannot run {}", self.cosign_path.display()), e))?;
        let out = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(r) => r.map_err(|e| unavailable("cannot wait for cosign", e))?,
            Err(_) => return Err(VerifyError::Unavailable("cosign timed out".into())),
        };
        if out.status.success() {
            Ok(())
        } else {
            Err(VerifyError::Rejected(format!(
                "cosign verify-blob exited with {}: {}",
                out.status,
                stderr_tail(&out.stderr)
            )))
        }
    }
}

/// Test-only trust root: the bundle is the lowercase hex of the 64-byte
/// ed25519 signature over the exact manifest bytes, with an optional
/// trailing newline.
#[cfg(feature = "test-verifier")]
#[derive(Debug, Clone)]
pub struct TestKey {
    key: ed25519_dalek::VerifyingKey,
}

#[cfg(feature = "test-verifier")]
impl TestKey {
    /// `public_key_hex`: 64 lowercase hex characters (32 bytes).
    pub fn from_hex(public_key_hex: &str) -> Result<Self, VerifyError> {
        let bytes: [u8; 32] = decode_lower_hex(public_key_hex.as_bytes()).ok_or_else(|| {
            VerifyError::Unavailable("test public key must be 64 lowercase hex characters".into())
        })?;
        let key = ed25519_dalek::VerifyingKey::from_bytes(&bytes)
            .map_err(|e| VerifyError::Unavailable(format!("invalid test public key: {e}")))?;
        Ok(TestKey { key })
    }
}

#[cfg(feature = "test-verifier")]
#[async_trait]
impl ReleaseVerifier for TestKey {
    async fn verify(&self, manifest: &[u8], bundle: &[u8]) -> Result<(), VerifyError> {
        let text = bundle.strip_suffix(b"\n").unwrap_or(bundle);
        let sig: [u8; 64] = decode_lower_hex(text).ok_or_else(|| {
            VerifyError::Rejected("bundle is not 128 lowercase hex characters".into())
        })?;
        self.key
            .verify_strict(manifest, &ed25519_dalek::Signature::from_bytes(&sig))
            .map_err(|_| {
                VerifyError::Rejected("ed25519 signature does not match the manifest".into())
            })
    }
}

#[cfg(feature = "test-verifier")]
fn decode_lower_hex<const N: usize>(s: &[u8]) -> Option<[u8; N]> {
    fn nibble(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        }
    }
    if s.len() != 2 * N {
        return None;
    }
    let mut out = [0u8; N];
    for (o, pair) in out.iter_mut().zip(s.chunks_exact(2)) {
        *o = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cosign(path: &str) -> CosignKeyless {
        CosignKeyless::new(
            path.into(),
            r"^https://github\.com/ElectricJack/agent-q/\.github/workflows/release\.yml@refs/tags/v[0-9]+\.[0-9]+\.[0-9]+$".into(),
            "https://token.actions.githubusercontent.com".into(),
        )
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn manifest_json(version: &str, source_revision: &str) -> Vec<u8> {
        let zero = "0".repeat(64);
        format!(
            r#"{{"manifest_version":1,"version":"{version}","source_revision":"{source_revision}",
"images":{{"server":"ghcr.io/electricjack/agent-q-server@sha256:{zero}","worker":"ghcr.io/electricjack/agent-q-worker@sha256:{zero}"}},
"protocol":{{"min":1,"max":1}},"state_format":1,"readable_state_formats":[1],
"upgrade_from":">=0.1.0","created_at":"2026-10-30T12:00:00Z"}}"#
        )
        .into_bytes()
    }

    #[test]
    fn binding_comes_from_the_manifest() {
        assert_eq!(
            release_binding(&manifest_json("1.2.3", SHA)).unwrap(),
            ReleaseBinding {
                tag_ref: "refs/tags/v1.2.3".into(),
                sha: SHA.into(),
            }
        );
    }

    #[test]
    fn binding_refuses_unparsable_manifests_and_odd_revisions() {
        for bad in [
            b"{}".to_vec(),
            b"not json".to_vec(),
            manifest_json("1.2.3", "rev-1.2.3"),
            manifest_json("1.2.3", &SHA.to_uppercase()),
            manifest_json("1.2.3", &SHA[1..]),
            manifest_json("1.2.3", &format!("{SHA}0")),
            manifest_json("1.2.3", &format!("-{}", &SHA[1..])),
        ] {
            let e = release_binding(&bad).unwrap_err();
            assert!(
                matches!(e, VerifyError::Rejected(_)),
                "{}: {e}",
                String::from_utf8_lossy(&bad)
            );
        }
    }

    #[test]
    fn cosign_argv_is_fixed() {
        let c = cosign("/usr/local/bin/cosign");
        let binding = release_binding(&manifest_json("1.2.3", SHA)).unwrap();
        let args = c.args(Path::new("/t/b.json"), Path::new("/t/m.json"), &binding);
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "verify-blob",
                "--bundle",
                "/t/b.json",
                "--certificate-identity-regexp",
                r"^https://github\.com/ElectricJack/agent-q/\.github/workflows/release\.yml@refs/tags/v[0-9]+\.[0-9]+\.[0-9]+$",
                "--certificate-oidc-issuer",
                "https://token.actions.githubusercontent.com",
                "--certificate-github-workflow-ref",
                "refs/tags/v1.2.3",
                "--certificate-github-workflow-sha",
                SHA,
                "/t/m.json",
            ]
        );
    }

    #[tokio::test]
    async fn cosign_refuses_an_unparsable_manifest_before_running() {
        // The binary does not exist: a refusal here never ran it.
        let e = cosign("/nonexistent/cosign")
            .verify(b"{}", b"{}")
            .await
            .unwrap_err();
        assert!(matches!(e, VerifyError::Rejected(_)), "{e}");
    }

    #[tokio::test]
    async fn cosign_missing_binary_is_unavailable() {
        let e = cosign("/nonexistent/cosign")
            .verify(&manifest_json("1.2.3", SHA), b"{}")
            .await
            .unwrap_err();
        assert!(matches!(e, VerifyError::Unavailable(_)), "{e}");
    }

    #[tokio::test]
    async fn cosign_nonzero_exit_is_rejected() {
        // `false` ignores its arguments and exits 1; no real cosign runs.
        let Some(f) = ["/bin/false", "/usr/bin/false"]
            .into_iter()
            .find(|p| Path::new(p).exists())
        else {
            return;
        };
        let e = cosign(f)
            .verify(&manifest_json("1.2.3", SHA), b"{}")
            .await
            .unwrap_err();
        assert!(matches!(e, VerifyError::Rejected(_)), "{e}");
        assert!(e.to_string().contains("exited with"), "{e}");
    }

    #[tokio::test]
    async fn cosign_timeout_kills_and_is_unavailable() {
        // A stand-in that ignores its arguments and hangs.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("hang");
        let pidfile = dir.path().join("pid");
        crate::test_support::write_stub(
            &script,
            &format!(
                "#!/bin/sh\necho $$ > {}\nexec sleep 30\n",
                pidfile.display()
            ),
        );
        let mut c = cosign(script.to_str().unwrap());
        c.timeout = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let e = c
            .verify(&manifest_json("1.2.3", SHA), b"{}")
            .await
            .unwrap_err();
        assert_eq!(e, VerifyError::Unavailable("cosign timed out".into()));
        assert!(started.elapsed() < Duration::from_secs(10));
        // The child was killed, not left running.
        // The script may still be starting when the timeout fires.
        let poll_until = std::time::Instant::now() + Duration::from_secs(2);
        let pid = loop {
            match std::fs::read_to_string(&pidfile) {
                Ok(p) if !p.trim().is_empty() => break p,
                _ if std::time::Instant::now() < poll_until => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                other => panic!("no pid written by the stand-in: {other:?}"),
            }
        };
        let proc_dir = format!("/proc/{}", pid.trim());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while Path::new(&proc_dir).exists()
            && !std::fs::read_to_string(format!("{proc_dir}/stat"))
                .unwrap_or_default()
                .contains(") Z")
        {
            assert!(
                std::time::Instant::now() < deadline,
                "cosign child still running"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[cfg(feature = "test-verifier")]
    mod test_key {
        use super::super::*;
        use ed25519_dalek::{Signer, SigningKey};

        fn hex(b: &[u8]) -> String {
            b.iter().map(|x| format!("{x:02x}")).collect()
        }

        fn pair(seed: u8) -> (SigningKey, TestKey) {
            let sk = SigningKey::from_bytes(&[seed; 32]);
            let tk = TestKey::from_hex(&hex(sk.verifying_key().as_bytes())).unwrap();
            (sk, tk)
        }

        #[tokio::test]
        async fn accepts_signature_with_or_without_newline() {
            let (sk, tk) = pair(7);
            let sig = hex(&sk.sign(b"manifest").to_bytes());
            tk.verify(b"manifest", sig.as_bytes()).await.unwrap();
            tk.verify(b"manifest", format!("{sig}\n").as_bytes())
                .await
                .unwrap();
        }

        #[tokio::test]
        async fn rejects_tampering_wrong_key_and_bad_encoding() {
            let (sk, tk) = pair(7);
            let (_, other) = pair(8);
            let sig = hex(&sk.sign(b"manifest").to_bytes());
            let upper = sig.to_uppercase();
            let doubled = format!("{sig}\n\n");
            let cases: [(&TestKey, &[u8], &[u8]); 6] = [
                (&tk, b"manifest!", sig.as_bytes()),
                (&other, b"manifest", sig.as_bytes()),
                (&tk, b"manifest", upper.as_bytes()),
                (&tk, b"manifest", doubled.as_bytes()),
                (&tk, b"manifest", &sig.as_bytes()[2..]),
                (&tk, b"manifest", b""),
            ];
            for (i, (key, m, b)) in cases.into_iter().enumerate() {
                let r = key.verify(m, b).await;
                assert!(
                    matches!(r, Err(VerifyError::Rejected(_))),
                    "case {i}: {r:?}"
                );
            }
        }

        #[test]
        fn public_key_must_be_64_lowercase_hex() {
            let (sk, _) = pair(7);
            let h = hex(sk.verifying_key().as_bytes());
            assert!(TestKey::from_hex(&h).is_ok());
            for bad in [
                h.to_uppercase(),
                h[2..].to_string(),
                format!("{h}00"),
                "zz".repeat(32),
            ] {
                assert!(
                    matches!(TestKey::from_hex(&bad), Err(VerifyError::Unavailable(_))),
                    "{bad}"
                );
            }
        }
    }
}

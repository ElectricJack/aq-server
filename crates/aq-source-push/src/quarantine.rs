use crate::{GitManifest, PushError, SourceRetentionConfig, artifact_id, digest, hash_file};
use aq_worker_protocol::{
    AttemptRef, ObjectAlgorithm, ObjectId, SourceBinding, SourceRole, TransferManifest,
};
use axum::{
    body::Body,
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use http_body_util::BodyExt;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::{Duration, SystemTime},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
};
use uuid::Uuid;

type Result<T> = std::result::Result<T, PushError>;
/// aq-git refuses a manifest with more prerequisites (its `validate_manifest`).
const AQ_GIT_PREREQUISITES: usize = 64;
const MARKER: &str = "aq-last-used";

fn refused(s: &str) -> PushError {
    PushError::Refused(s.into())
}
fn root(cfg: &SourceRetentionConfig) -> PathBuf {
    cfg.staging_root.join("pushed")
}
/// Git with no inherited configuration, environment, hooks or prompts.
fn git(repo: &Path) -> Command {
    let mut c = Command::new("git");
    c.env_clear()
        .env(
            "PATH",
            std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["-c", "core.hooksPath=/dev/null", "-c", "gc.auto=0"])
        .arg("--git-dir")
        .arg(repo)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    c
}
async fn run(repo: &Path, args: &[&str]) -> Result<Output> {
    Ok(git(repo).args(args).output().await?)
}
async fn must(repo: &Path, args: &[&str]) -> Result<String> {
    let out = run(repo, args).await?;
    if !out.status.success() {
        return Err(PushError::Internal(anyhow::anyhow!(
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}
fn tree_bytes(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path)
        .map(|entries| entries.flatten().map(|e| tree_bytes(&e.path())).sum())
        .unwrap_or(0)
}
async fn bytes_under(path: PathBuf) -> Result<u64> {
    tokio::task::spawn_blocking(move || tree_bytes(&path))
        .await
        .map_err(|e| PushError::Internal(e.into()))
}
/// Bytes held by every attempt's quarantine. They count against the staging
/// quota next to the transfers the database accounts for.
pub async fn usage(cfg: &SourceRetentionConfig) -> Result<u64> {
    bytes_under(root(cfg)).await
}
/// Removes quarantines nobody pushed to or admitted from for the incomplete
/// TTL. A quarantine holds only what a worker can push again.
pub async fn expire(cfg: &SourceRetentionConfig) -> Result<()> {
    let (root, ttl) = (root(cfg), Duration::from_secs(cfg.incomplete_ttl_secs));
    tokio::task::spawn_blocking(move || {
        let Ok(entries) = std::fs::read_dir(&root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let used = std::fs::metadata(path.join(MARKER))
                .or_else(|_| std::fs::symlink_metadata(&path))
                .and_then(|m| m.modified());
            let stale = used.is_ok_and(|at| {
                SystemTime::now()
                    .duration_since(at)
                    .is_ok_and(|age| age > ttl)
            });
            if stale && let Err(e) = std::fs::remove_dir_all(&path) {
                tracing::warn!(error=%e, path=%path.display(), "stale push quarantine kept");
            }
        }
    })
    .await
    .map_err(|e| PushError::Internal(e.into()))
}

/// One attempt's quarantine: a bare repository that borrows the central pool's
/// objects read-only and holds only what that attempt pushed.
pub struct Quarantine {
    repo: PathBuf,
    attempt: Uuid,
    base: String,
}
impl Quarantine {
    pub fn at(cfg: &SourceRetentionConfig, attempt: Uuid, source: &SourceBinding) -> Result<Self> {
        let base = &source.base;
        // The base is written into a Git configuration value below.
        if base.algorithm != ObjectAlgorithm::Sha1
            || base.hex.len() != 40
            || !base
                .hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(refused("source binding has no SHA-1 base commit"));
        }
        Ok(Self {
            repo: root(cfg).join(format!("{attempt}.git")),
            attempt,
            base: base.hex.clone(),
        })
    }
    fn attempt_ref(&self) -> String {
        format!("refs/aq/attempts/{}", self.attempt)
    }
    pub fn exists(&self) -> bool {
        self.repo.join("objects").is_dir()
    }
    async fn touch(&self) {
        if let Err(e) = tokio::fs::write(self.repo.join(MARKER), b"").await {
            tracing::warn!(error=%e, "push quarantine use not recorded");
        }
    }
    /// Opens the attempt's quarantine for a push, creating it on first use, and
    /// returns how many bytes that push may send. `staged` is what the database
    /// already charges to the staging quota.
    pub async fn open(
        cfg: &SourceRetentionConfig,
        attempt: Uuid,
        source: &SourceBinding,
        staged: u64,
    ) -> Result<(Self, u64)> {
        let q = Self::at(cfg, attempt, source)?;
        if !q.exists() {
            q.create(cfg, source).await?;
        }
        q.touch().await;
        let own = bytes_under(q.repo.clone()).await?;
        let free = cfg
            .max_staging_bytes
            .saturating_sub(staged)
            .saturating_sub(usage(cfg).await?);
        let limit = cfg.max_bundle_bytes.saturating_sub(own).min(free);
        if limit == 0 {
            return Err(PushError::TooLarge(
                "push quarantine or staging quota exhausted".into(),
            ));
        }
        Ok((q, limit))
    }
    async fn create(&self, cfg: &SourceRetentionConfig, source: &SourceBinding) -> Result<()> {
        let pool = cfg
            .git_root
            .join("repositories")
            .join(source.repository_id.to_string())
            .join("objects.git/objects");
        if !pool.is_dir() {
            return Err(refused("central pool of the repository is absent"));
        }
        let root = root(cfg);
        tokio::fs::create_dir_all(&root).await?;
        let tmp = root.join(format!(".new-{}-{}", self.attempt, Uuid::new_v4()));
        let made = async {
            let init = Command::new("git")
                .env_clear()
                .env(
                    "PATH",
                    std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
                )
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(["init", "--quiet", "--bare", "--object-format=sha1"])
                .arg("--template=")
                .arg(&tmp)
                .stdin(Stdio::null())
                .output()
                .await?;
            if !init.status.success() {
                return Err(PushError::Internal(anyhow::anyhow!(
                    "quarantine init failed: {}",
                    String::from_utf8_lossy(&init.stderr).trim()
                )));
            }
            tokio::fs::create_dir_all(tmp.join("objects/info")).await?;
            tokio::fs::write(
                tmp.join("objects/info/alternates"),
                format!("{}\n", pool.display()),
            )
            .await?;
            let commit = format!("{}^{{commit}}", self.base);
            if !run(&tmp, &["cat-file", "-e", &commit])
                .await?
                .status
                .success()
            {
                return Err(refused("central pool does not hold the base commit"));
            }
            Ok(())
        }
        .await;
        // A concurrent first push of the same attempt may have won the rename.
        if made.is_err() || tokio::fs::rename(&tmp, &self.repo).await.is_err() {
            let _ = tokio::fs::remove_dir_all(&tmp).await;
        }
        made?;
        if !self.exists() {
            return Err(PushError::Internal(anyhow::anyhow!(
                "push quarantine was not created"
            )));
        }
        Ok(())
    }
    /// `git receive-pack`, scoped so that the only ref this attempt can see or
    /// write is its own. The base is offered as a `.have` line, which lets the
    /// client send a thin pack without the repository holding a second ref.
    fn receive_pack(&self, limit: u64) -> Command {
        let mut c = git(&self.repo);
        for setting in [
            "receive.hideRefs=refs/".to_owned(),
            format!("receive.hideRefs=!{}", self.attempt_ref()),
            format!("core.alternateRefsCommand=echo {} #", self.base),
            "receive.fsckObjects=true".to_owned(),
            "receive.denyDeletes=true".to_owned(),
            "receive.denyNonFastForwards=true".to_owned(),
            "receive.autogc=false".to_owned(),
            "receive.unpackLimit=1".to_owned(),
            format!("receive.maxInputSize={limit}"),
        ] {
            c.arg("-c").arg(setting);
        }
        c.args(["receive-pack", "--stateless-rpc"]);
        c
    }
    /// The `info/refs?service=git-receive-pack` response.
    pub async fn advertise(&self) -> Result<Response> {
        let out = self
            .receive_pack(1)
            .arg("--advertise-refs")
            .arg(&self.repo)
            .output()
            .await?;
        if !out.status.success() {
            return Err(PushError::Internal(anyhow::anyhow!(
                "ref advertisement failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let mut body = b"001f# service=git-receive-pack\n0000".to_vec();
        body.extend_from_slice(&out.stdout);
        Ok(reply("application/x-git-receive-pack-advertisement", body))
    }
    /// The `git-receive-pack` POST: the request body is streamed into
    /// `git receive-pack`, never held in memory, and stops at `limit` bytes.
    pub async fn receive(
        &self,
        cfg: &SourceRetentionConfig,
        headers: &HeaderMap,
        body: Body,
        limit: u64,
    ) -> Result<Response> {
        let text = |name: header::HeaderName| headers.get(name).and_then(|v| v.to_str().ok());
        if text(header::CONTENT_TYPE) != Some("application/x-git-receive-pack-request") {
            return Err(PushError::Invalid(
                "content type must be application/x-git-receive-pack-request".into(),
            ));
        }
        if text(header::CONTENT_ENCODING).is_some_and(|v| !v.eq_ignore_ascii_case("identity")) {
            return Err(PushError::Invalid(
                "only identity content encoding is accepted".into(),
            ));
        }
        let too_large = || PushError::TooLarge("push exceeds the configured size limit".into());
        if text(header::CONTENT_LENGTH)
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|n| n > limit)
        {
            return Err(too_large());
        }
        let mut child = self
            .receive_pack(limit)
            .arg(&self.repo)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        // Dropping stdin at the end of `feed` is what lets Git finish or fail.
        let feed = async move {
            let (mut body, mut sent) = (body, 0u64);
            while let Some(frame) = body.frame().await {
                let Ok(frame) = frame else {
                    return Err(PushError::Invalid("request body ended early".into()));
                };
                let Ok(data) = frame.into_data() else {
                    continue;
                };
                sent = sent.saturating_add(data.len() as u64);
                if sent > limit {
                    return Err(PushError::TooLarge(
                        "push exceeds the configured size limit".into(),
                    ));
                }
                // Git closed its input: its own report says why.
                if stdin.write_all(&data).await.is_err() {
                    break;
                }
            }
            Ok(())
        };
        let exchange = async {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let (fed, _, _) = tokio::join!(
                feed,
                stdout.read_to_end(&mut out),
                stderr.read_to_end(&mut err)
            );
            let status = child.wait().await?;
            Ok::<_, PushError>((fed, status, out, err))
        };
        let (fed, status, out, err) =
            tokio::time::timeout(Duration::from_secs(cfg.incomplete_ttl_secs), exchange)
                .await
                .map_err(|_| refused("push did not complete in time"))??;
        self.touch().await;
        fed?;
        let err = String::from_utf8_lossy(&err);
        if err.contains("exceeds maximum allowed size") {
            return Err(too_large());
        }
        if !status.success() {
            tracing::warn!(attempt=%self.attempt, stderr=%err.trim(), "receive-pack refused a push");
            return Err(PushError::Invalid(
                "git receive-pack refused the request".into(),
            ));
        }
        if bytes_under(self.repo.clone()).await? > cfg.max_bundle_bytes {
            self.remove().await;
            return Err(too_large());
        }
        Ok(reply("application/x-git-receive-pack-result", out))
    }
    fn built(&self, transfer_id: Uuid) -> PathBuf {
        self.repo.join(format!("aq-artifact-{transfer_id}"))
    }
    /// Moves a built artifact to the transfer's staging directory. It does
    /// nothing when there is none left to move, so a retry after a crash on
    /// either side of the move continues from where the first try stopped.
    pub async fn promote(&self, transfer_id: Uuid, staging: &Path) -> Result<()> {
        let built = self.built(transfer_id);
        if tokio::fs::try_exists(&built).await? && !tokio::fs::try_exists(staging).await? {
            tokio::fs::rename(&built, staging).await?;
        }
        Ok(())
    }
    /// Builds the aq-git artifact for `checkpoint` (`manifest.json` and
    /// `objects.bundle`) inside the quarantine, where it expires with it until
    /// `promote` hands it over, and returns the transfer manifest describing it.
    /// Refuses unless the attempt's pushed ref is exactly `checkpoint`, a commit
    /// that descends from the binding's base.
    pub async fn artifact(
        &self,
        cfg: &SourceRetentionConfig,
        transfer_id: Uuid,
        attempt: AttemptRef,
        resolved_spec_digest: String,
        source: SourceBinding,
        checkpoint: ObjectId,
    ) -> Result<TransferManifest> {
        if !self.exists() {
            return Err(refused("nothing has been pushed for this attempt"));
        }
        self.touch().await;
        let repo = &self.repo;
        let pushed = run(repo, &["rev-parse", "--verify", "-q", &self.attempt_ref()]).await?;
        let pushed = String::from_utf8_lossy(&pushed.stdout).trim().to_owned();
        if pushed.is_empty() {
            return Err(refused("nothing has been pushed for this attempt"));
        }
        if checkpoint.algorithm != ObjectAlgorithm::Sha1 || pushed != checkpoint.hex {
            return Err(refused("checkpoint is not the pushed attempt ref"));
        }
        if must(repo, &["cat-file", "-t", &pushed]).await? != "commit" {
            return Err(refused("pushed attempt ref is not a commit"));
        }
        let ancestry = run(repo, &["merge-base", "--is-ancestor", &self.base, &pushed]).await?;
        match ancestry.status.code() {
            Some(0) => {}
            Some(1) => return Err(refused("base is not an ancestor of the checkpoint")),
            _ => {
                return Err(PushError::Internal(anyhow::anyhow!(
                    "ancestry check failed: {}",
                    String::from_utf8_lossy(&ancestry.stderr).trim()
                )));
            }
        }
        let artifact = artifact_id(transfer_id);
        let reference = format!("refs/aq-git/transfers/{artifact}");
        must(repo, &["update-ref", &reference, &pushed]).await?;
        let dir = &self.built(transfer_id);
        if tokio::fs::try_exists(dir).await? {
            tokio::fs::remove_dir_all(dir).await?;
        }
        tokio::fs::create_dir_all(dir).await?;
        let bundle = dir.join("objects.bundle");
        let bundle_path = bundle.to_string_lossy().into_owned();
        let exclude = format!("^{}", self.base);
        let mut args = vec!["bundle", "create", "--quiet", &bundle_path, &reference];
        // With nothing after the base there is nothing to exclude it from, so
        // the bundle carries the whole history and needs no prerequisite.
        if pushed != self.base {
            args.push(&exclude);
        }
        must(repo, &args).await?;
        let prerequisites = header(&bundle, &pushed, &reference).await?;
        if prerequisites.len() > AQ_GIT_PREREQUISITES {
            return Err(refused("checkpoint has too many boundary commits"));
        }
        let bundle_bytes = tokio::fs::metadata(&bundle).await?.len();
        if bundle_bytes > cfg.max_bundle_bytes {
            tokio::fs::remove_dir_all(dir).await?;
            return Err(PushError::TooLarge("bundle limit exceeded".into()));
        }
        let bundle_digest = hash_file(&bundle).await?;
        let format: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(cfg.git_root.join("format.json")).await?)
                .map_err(|e| PushError::Internal(e.into()))?;
        let Some(installation) = format["installation_id"].as_str() else {
            return Err(PushError::Internal(anyhow::anyhow!(
                "aq-git root has no installation id"
            )));
        };
        let manifest = GitManifest {
            format_version: 1,
            repository_id: source.repository_id.to_string(),
            repository_identity: source.logical_identity.clone(),
            binding_generation: source.binding_generation,
            transfer_id: artifact.to_string(),
            object_format: "sha1".into(),
            base: source.base.clone(),
            source: checkpoint.clone(),
            prerequisites,
            bundle_size: bundle_bytes,
            bundle_sha256: Some(bundle_digest.clone()),
            producer_installation_id: installation.to_owned(),
            reference_only: false,
            caller_correlation: json!({
                "owner": "aq-server.source-push",
                "attempt_id": attempt.attempt_id,
                "transfer_id": transfer_id,
            }),
        };
        let bytes = serde_json::to_vec(&manifest).map_err(|e| PushError::Internal(e.into()))?;
        let mut file = tokio::fs::File::create(dir.join("manifest.json")).await?;
        file.write_all(&bytes).await?;
        file.sync_all().await?;
        tokio::fs::File::open(&bundle).await?.sync_all().await?;
        Ok(TransferManifest {
            transfer_id,
            role: SourceRole::Checkpoint,
            checkpoint,
            attempt,
            resolved_spec_digest,
            source,
            artifact_id: artifact,
            manifest_digest: digest(&bytes),
            bundle_digest: Some(bundle_digest),
            manifest_bytes: bytes.len() as u64,
            bundle_bytes,
        })
    }
    /// Best effort: what is left is removed again when it goes stale.
    pub async fn remove(&self) {
        if let Err(e) = tokio::fs::remove_dir_all(&self.repo).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error=%e, attempt=%self.attempt, "push quarantine cleanup deferred");
        }
    }
}
fn reply(content_type: &'static str, body: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}
/// Prerequisites a v2 bundle lists, after checking it advertises exactly
/// `reference` at `source`. The manifest must repeat what the header says.
async fn header(bundle: &Path, source: &str, reference: &str) -> Result<Vec<ObjectId>> {
    let malformed = || PushError::Internal(anyhow::anyhow!("unexpected bundle header"));
    let mut reader = BufReader::new(tokio::fs::File::open(bundle).await?);
    let (mut prerequisites, mut refs) = (Vec::new(), Vec::new());
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).await?;
    if line != b"# v2 git bundle\n" {
        return Err(malformed());
    }
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).await? == 0 {
            return Err(malformed());
        }
        // A prerequisite line ends with the commit subject, which is any bytes.
        let text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
        if text.is_empty() {
            break;
        }
        match text.strip_prefix('-') {
            Some(p) => prerequisites.push(ObjectId {
                algorithm: ObjectAlgorithm::Sha1,
                hex: p.split(' ').next().unwrap_or_default().to_owned(),
            }),
            None => refs.push(text),
        }
    }
    if refs != [format!("{source} {reference}")] {
        return Err(malformed());
    }
    Ok(prerequisites)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(dir: &Path, ttl: u64) -> SourceRetentionConfig {
        serde_json::from_value(json!({
            "staging_root": dir.join("staging"),
            "git_root": dir.join("git"),
            "git_socket": dir.join("git.sock"),
            "incomplete_ttl_secs": ttl,
        }))
        .unwrap()
    }
    #[tokio::test]
    async fn only_quarantines_unused_for_the_ttl_expire() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), 3600);
        let (fresh, stale) = (root(&cfg).join("fresh.git"), root(&cfg).join("stale.git"));
        for repo in [&fresh, &stale] {
            std::fs::create_dir_all(repo.join("objects")).unwrap();
            std::fs::write(repo.join("objects/pack"), vec![0u8; 1000]).unwrap();
            std::fs::write(repo.join(MARKER), b"").unwrap();
        }
        assert!(usage(&cfg).await.unwrap() >= 2000);
        let old = SystemTime::now() - Duration::from_secs(7200);
        std::fs::File::options()
            .write(true)
            .open(stale.join(MARKER))
            .unwrap()
            .set_modified(old)
            .unwrap();
        expire(&cfg).await.unwrap();
        assert!(fresh.exists() && !stale.exists());
        // No quarantine was ever created: nothing to do, nothing to fail.
        expire(&config(&dir.path().join("none"), 1)).await.unwrap();
        assert_eq!(
            usage(&config(&dir.path().join("none"), 1)).await.unwrap(),
            0
        );
    }
    #[test]
    fn a_base_that_is_not_a_sha1_commit_id_never_reaches_git_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), 3600);
        let mut source: SourceBinding = serde_json::from_value(json!({
            "repository_id": Uuid::from_u128(1),
            "binding_generation": 1,
            "logical_identity": "example/repository",
            "base": {"algorithm": "sha1", "hex": "a".repeat(40)},
        }))
        .unwrap();
        assert!(Quarantine::at(&cfg, Uuid::from_u128(2), &source).is_ok());
        for hex in [
            "a".repeat(39),
            format!("{};x #", "a".repeat(35)),
            "A".repeat(40),
        ] {
            source.base.hex = hex;
            assert!(matches!(
                Quarantine::at(&cfg, Uuid::from_u128(2), &source),
                Err(PushError::Refused(_))
            ));
        }
    }
}

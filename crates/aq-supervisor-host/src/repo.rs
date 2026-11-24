//! `repo sync`: a read-only project checkout through the supervisor git route.
//! The bearer reaches git only through GIT_CONFIG_* on the child's environment.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::AsyncReadExt;
use uuid::Uuid;

/// Clones or fetches the project's repository and prints `<path> <main head>`.
pub async fn sync(project: Uuid, workspace: &Path) -> Result<PathBuf> {
    let (origin, token) = crate::tool::machine()?;
    let (name, repository, remote) = project_remote(&origin, &token, project).await?;
    let dir = checkout_dir(workspace, &name, repository);
    let head = sync_from(remote.as_str(), std::str::from_utf8(&token)?, &dir)?;
    println!("{} {head}", dir.display());
    Ok(dir)
}

async fn project_remote(
    origin: &reqwest::Url,
    token: &[u8],
    project: Uuid,
) -> Result<(String, Uuid, reqwest::Url)> {
    let token = std::str::from_utf8(token)?;
    ensure!(
        !token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic()),
        "invalid supervisor bearer"
    );
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(60))
        .build()?
        .post(origin.join("api/v1/commands/project.show")?)
        .bearer_auth(token)
        .json(&json!({"params":{"project_id":project}}))
        .send()
        .await
        .context("project.show transport failed")?;
    ensure!(
        response.status().is_success(),
        "AQ refused project.show ({})",
        response.status()
    );
    let record = crate::tool::bounded_json(response, 2_097_152).await?;
    let name = record["binding"]["name"].as_str().context("project name")?;
    let repository: Uuid = record["binding"]["repository_id"]
        .as_str()
        .context("project repository")?
        .parse()?;
    let remote = origin.join(&format!("v1/supervisor/git/{repository}"))?;
    Ok((name.to_owned(), repository, remote))
}

/// The three entries that hand git the bearer as an `http.extraHeader`.
pub fn git_env(token: &str) -> Vec<(String, String)> {
    vec![
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        ("GIT_CONFIG_KEY_0".into(), "http.extraHeader".into()),
        (
            "GIT_CONFIG_VALUE_0".into(),
            format!("Authorization: Bearer {token}"),
        ),
    ]
}

/// `workspace/repos/<name>-<repository id prefix>`: the name reduced to one safe
/// path segment, the id making the folder unique to the repository.
pub fn checkout_dir(workspace: &Path, project_name: &str, repository: Uuid) -> PathBuf {
    let mut name: String = project_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.trim_matches('.').is_empty() {
        name = "_".into();
    }
    let id = &repository.simple().to_string()[..8];
    workspace.join("repos").join(format!("{name}-{id}"))
}

const REFSPECS: [&str; 2] = [
    "+refs/heads/main:refs/remotes/origin/main",
    "+refs/heads/aq/*:refs/remotes/origin/aq/*",
];

/// Returns the fetched `origin/main` commit.
fn sync_from(remote: &str, token: &str, dir: &Path) -> Result<String> {
    if !dir.join(".git").is_dir() {
        std::fs::create_dir_all(dir.parent().context("checkout parent")?)?;
        let target = dir.to_str().context("checkout path")?;
        git(
            token,
            None,
            &["clone", "-q", "--origin", "origin", remote, target],
        )?;
    }
    // The route can change between syncs; never fetch from a stored old URL.
    git(token, Some(dir), &["remote", "set-url", "origin", remote])?;
    let mut fetch = vec!["fetch", "-q", "--prune", "origin"];
    fetch.extend(REFSPECS);
    git(token, Some(dir), &fetch)?;
    // Untracked files never block the move; local edits to tracked files do.
    if git(
        token,
        Some(dir),
        &["status", "--porcelain", "--untracked-files=no"],
    )?
    .is_empty()
    {
        git(
            token,
            Some(dir),
            &["checkout", "-q", "--detach", "origin/main"],
        )?;
    } else {
        eprintln!("{} has local changes; left as it is", dir.display());
    }
    git(token, Some(dir), &["rev-parse", "origin/main"])
}

fn git(token: &str, dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut command = std::process::Command::new("git");
    command
        .env_clear()
        .envs(
            ["PATH", "HOME"]
                .iter()
                .filter_map(|k| Some((k, std::env::var_os(k)?))),
        )
        .env("GIT_TERMINAL_PROMPT", "0")
        .envs(git_env(token))
        .args(args)
        .stdin(std::process::Stdio::null());
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let output = command.output().context("git could not run")?;
    if !output.status.success() {
        // git does not echo http.extraHeader; the replace is belt and braces.
        let stderr = String::from_utf8_lossy(&output.stderr).replace(token, "***");
        let detail: String = stderr.trim().chars().take(2048).collect();
        anyhow::bail!("git {} failed: {detail}", args[0]);
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SyncArguments {
    pub project_id: Uuid,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadArguments {
    project_id: Uuid,
    path: String,
    #[serde(rename = "ref", default)]
    reference: Option<String>,
}

#[derive(Default)]
pub(crate) struct SessionRepositories {
    root: Option<PathBuf>,
    synced: HashSet<Uuid>,
}

impl Drop for SessionRepositories {
    fn drop(&mut self) {
        if let Some(root) = &self.root {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

impl SessionRepositories {
    pub(crate) async fn sync(&mut self, project_id: Uuid) -> Result<Value> {
        let (origin, token) = crate::tool::machine()?;
        let (_, _, remote) = project_remote(&origin, &token, project_id).await?;
        let token = std::str::from_utf8(&token)?;
        if self.root.is_none() {
            let root = Path::new("/workspace").join(format!(".aq-repo-{}", Uuid::new_v4()));
            std::fs::DirBuilder::new().mode(0o700).create(&root)?;
            self.root = Some(root);
        }
        let root = self.root.as_ref().context("repository session root")?;
        let dir = root.join(project_id.to_string());
        ensure!(
            dir.exists() || std::fs::read_dir(root)?.take(32).count() < 32,
            "repository session limit"
        );
        self.synced.remove(&project_id);
        if !dir.exists() {
            std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
            if let Err(error) = session_git(&dir, None, &["init", "--bare", "--template="]).await {
                std::fs::remove_dir_all(&dir)?;
                return Err(error);
            }
        }
        let mut fetch = vec![
            "fetch",
            "--quiet",
            "--prune",
            "--no-tags",
            "--",
            remote.as_str(),
        ];
        fetch.extend(REFSPECS);
        session_git(&dir, Some(token), &fetch).await?;
        let head = session_git(&dir, None, &["rev-parse", "refs/remotes/origin/main"]).await?;
        self.synced.insert(project_id);
        Ok(json!({"project_id":project_id,"head":head.trim()}))
    }

    pub(crate) async fn read(&self, arguments: ReadArguments) -> Result<Value> {
        ensure!(
            self.synced.contains(&arguments.project_id),
            "repository not synced"
        );
        let dir = self
            .root
            .as_ref()
            .context("repository session root")?
            .join(arguments.project_id.to_string());
        let reference = arguments.reference.as_deref().unwrap_or("main");
        let revision = match reference {
            "main" => "refs/remotes/origin/main".to_owned(),
            name => {
                let task = name
                    .strip_prefix("aq/")
                    .context("unsupported repository ref")?;
                ensure!(
                    Uuid::parse_str(task).is_ok_and(|id| id.to_string() == task),
                    "invalid task ref"
                );
                format!("refs/remotes/origin/aq/{task}")
            }
        };
        let path = arguments.path;
        ensure!(
            path.len() <= 4096
                && !path.chars().any(char::is_control)
                && (path.is_empty()
                    || path
                        .split('/')
                        .all(|part| { !part.is_empty() && !matches!(part, "." | ".." | ".git") })),
            "invalid repository path"
        );
        // Git reads only tracked objects at these refs. Symlink blobs are text
        // and no filesystem path from the caller is opened.
        let object = format!("{revision}:{path}");
        let kind = session_git(&dir, None, &["cat-file", "-t", &object]).await?;
        match kind.trim() {
            "tree" => {
                let entries =
                    session_git(&dir, None, &["ls-tree", "-z", "--name-only", &object]).await?;
                let entries: Vec<_> = entries.split_terminator('\0').collect();
                Ok(json!({"ref":reference,"path":path,"entries":entries}))
            }
            "blob" => {
                let text = session_git(&dir, None, &["cat-file", "blob", &object]).await?;
                ensure!(!text.contains('\0'), "repository blob is not text");
                Ok(json!({"ref":reference,"path":path,"text":text}))
            }
            _ => anyhow::bail!("unsupported repository object"),
        }
    }
}

async fn session_git(dir: &Path, token: Option<&str>, args: &[&str]) -> Result<String> {
    use tokio::process::Command;

    tokio::time::timeout(Duration::from_secs(60), async {
        let mut command = Command::new("git");
        command
            .env_clear()
            .envs(std::env::var_os("PATH").map(|path| ("PATH", path)))
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .arg("-c")
            .arg("http.followRedirects=false")
            .current_dir(dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(token) = token {
            command.envs(git_env(token));
        }
        let mut child = command.spawn()?;
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .context("git stdout")?
            .take(131_073)
            .read_to_end(&mut output)
            .await?;
        ensure!(output.len() <= 131_072, "repository output byte bound");
        ensure!(child.wait().await?.success(), "repository command failed");
        Ok(String::from_utf8(output)?)
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_env_carries_the_bearer_only_in_env() {
        let pair = |k: &str, v: &str| (k.to_owned(), v.to_owned());
        assert_eq!(
            git_env("tok"),
            [
                pair("GIT_CONFIG_COUNT", "1"),
                pair("GIT_CONFIG_KEY_0", "http.extraHeader"),
                pair("GIT_CONFIG_VALUE_0", "Authorization: Bearer tok"),
            ]
        );
    }

    #[test]
    fn checkout_dir_sanitizes_names() {
        let w = Path::new("/w");
        let id = Uuid::parse_str("0123abcd-0000-0000-0000-000000000000").unwrap();
        assert_eq!(
            checkout_dir(w, "aq-server", id),
            Path::new("/w/repos/aq-server-0123abcd")
        );
        let other = Uuid::parse_str("ffff0000-0000-0000-0000-000000000000").unwrap();
        assert_ne!(checkout_dir(w, "same", id), checkout_dir(w, "same", other));
        for bad in ["../x", "a/b", "..", ".", "", "/"] {
            let dir = checkout_dir(w, bad, id);
            assert_eq!(dir.parent(), Some(Path::new("/w/repos")), "{bad}");
            assert!(!matches!(
                dir.file_name().unwrap().to_str(),
                Some(".." | ".")
            ));
        }
    }

    fn run(dir: &Path, args: &[&str]) -> String {
        git("unused", Some(dir), args).unwrap()
    }

    #[test]
    fn sync_clones_then_fetches() {
        let root = tempfile::tempdir().unwrap();
        let (bare, work) = (root.path().join("bare.git"), root.path().join("src"));
        std::fs::create_dir_all(&work).unwrap();
        run(
            root.path(),
            &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        run(&work, &["init", "-q", "-b", "main"]);
        let commit = |message: &str| {
            run(
                &work,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    message,
                ],
            );
            run(&work, &["rev-parse", "HEAD"])
        };
        let first = commit("one");
        let url = format!("file://{}", bare.display());
        run(&work, &["push", "-q", &url, "main", "main:aq/task-1"]);
        let dir = checkout_dir(root.path(), "proj", Uuid::nil());
        assert_eq!(sync_from(&url, "unused", &dir).unwrap(), first);
        assert_eq!(run(&dir, &["rev-parse", "origin/aq/task-1"]), first);
        std::fs::write(dir.join("notes.txt"), "mine").unwrap();
        let second = commit("two");
        run(&work, &["push", "-q", &url, "main", "main:aq/task-2"]);
        assert_eq!(sync_from(&url, "unused", &dir).unwrap(), second);
        assert_eq!(run(&dir, &["rev-parse", "HEAD"]), second);
        assert_eq!(run(&dir, &["rev-parse", "origin/aq/task-2"]), second);
        assert_eq!(
            std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
            "mine"
        );

        // A deleted aq/* branch is pruned on the next sync.
        run(&work, &["push", "-q", &url, ":aq/task-1"]);
        sync_from(&url, "unused", &dir).unwrap();
        assert!(!run(&dir, &["branch", "-r"]).contains("origin/aq/task-1"));
        assert!(run(&dir, &["branch", "-r"]).contains("origin/aq/task-2"));

        // A clone whose stored origin is stale is re-pointed before fetching.
        run(
            &dir,
            &["remote", "set-url", "origin", "file:///nonexistent"],
        );
        assert_eq!(sync_from(&url, "unused", &dir).unwrap(), second);
        assert_eq!(run(&dir, &["remote", "get-url", "origin"]), url);
    }

    #[tokio::test]
    async fn session_reads_only_tracked_objects_from_synced_projects() {
        let root = tempfile::tempdir().unwrap();
        let project_id = Uuid::new_v4();
        let dir = root.path().join(project_id.to_string());
        std::fs::create_dir(&dir).unwrap();
        run(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("readme"), "committed").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.join("link")).unwrap();
        run(&dir, &["add", "readme", "link"]);
        run(
            &dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "test",
            ],
        );
        run(&dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        std::fs::write(dir.join("readme"), "local edit").unwrap();
        let repositories = SessionRepositories {
            root: Some(root.path().to_owned()),
            synced: HashSet::from([project_id]),
        };
        let read = |path: &str| {
            repositories.read(ReadArguments {
                project_id,
                path: path.to_owned(),
                reference: None,
            })
        };
        assert_eq!(
            read("").await.unwrap()["entries"],
            json!(["link", "readme"])
        );
        assert_eq!(read("readme").await.unwrap()["text"], "committed");
        assert_eq!(read("link").await.unwrap()["text"], "/etc/passwd");
        for path in ["../readme", "/etc/passwd", ".git/config", "link/passwd"] {
            assert!(read(path).await.is_err(), "{path}");
        }
        assert!(
            repositories
                .read(ReadArguments {
                    project_id: Uuid::new_v4(),
                    path: String::new(),
                    reference: None,
                })
                .await
                .is_err()
        );
    }

    #[test]
    fn failed_git_reports_stderr_without_the_token() {
        let root = tempfile::tempdir().unwrap();
        let dir = checkout_dir(root.path(), "p", Uuid::nil());
        let err = sync_from("file:///nonexistent/repo.git", "SECRETTOKEN", &dir)
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("git clone failed: "), "{err}");
        assert!(
            err.len() > "git clone failed: ".len() && err.len() < 2100,
            "{err}"
        );
        assert!(!err.contains("SECRETTOKEN"));
    }
}

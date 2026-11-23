//! Spec 0017 §6.3-6.4: what host catalog 2.5.0's `environment.build` and
//! `environment.admit` do, and the base changes the delivery playbook hears
//! of. The playbook host records the operations and seals their completions;
//! this module decides each answer.
use crate::{store, worker};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use std::path::Path;
use uuid::Uuid;

/// Where recipes live in a project's repository: `<name>.md` each.
pub const RECIPES: [&str; 2] = [".aq", "environments"];
/// The catalog's bounds on a build summary (contract `environment.build`).
const MAX_BUILDS: usize = 32;
const MAX_TEXT: usize = 128;
const MAX_LOG: usize = 8192;

/// How a binding change went.
pub enum Bound {
    Done,
    /// The binding refuses the change (it would not validate, or is gone).
    Refused,
    /// Try again later (a concurrent change, or a peer unavailable).
    Retry,
}
/// What the commands need of the server.
#[async_trait::async_trait]
pub trait EnvironmentOps: Send + Sync {
    /// `(name, git blob id)` of every `.aq/environments/<name>.md` at
    /// `commit` in the project's repository; `None` when aq-git cannot say.
    async fn recipes(&self, project: Uuid, commit: &str) -> Option<Vec<(String, String)>>;
    /// Adds `environments` to the project's binding and, given one, makes
    /// `default` its default, as `actor` (idempotent under `operation`).
    async fn bind(
        &self,
        actor: Uuid,
        project: Uuid,
        environments: &[String],
        default: Option<&str>,
        operation: Uuid,
    ) -> Bound;
}

/// The recipes at `commit` through aq-git's `tree` query (no blob is read):
/// the commit's tree, then `.aq`, then `environments`.
pub async fn recipes_at(
    socket: &Path,
    repository: Uuid,
    generation: u64,
    commit: &str,
) -> Option<Vec<(String, String)>> {
    use aq_delivery::repository_onboarding::{Git, aq_git};
    let mut oid = commit.to_owned();
    for directory in RECIPES.iter().map(Some).chain([None]) {
        let params = json!({"oid": {"algorithm": "sha1", "hex": oid}, "limit": 1000});
        let id = Uuid::new_v4();
        let Git::Ok(listed) =
            aq_git(Some(socket), id, "tree", repository, generation, params).await
        else {
            return None;
        };
        let entries = listed["entries"].as_array()?;
        let path = |e: &Value| e["path"]["value"].as_str().map(str::to_owned);
        let Some(directory) = directory else {
            let recipes = entries.iter().filter(|e| e["type"] == "blob");
            return Some(
                recipes
                    .filter_map(|e| {
                        let name = path(e)?.strip_suffix(".md")?.to_owned();
                        Some((name, e["oid"]["hex"].as_str()?.to_owned()))
                    })
                    .collect(),
            );
        };
        let next = entries
            .iter()
            .find(|e| e["type"] == "tree" && path(e).as_deref() == Some(*directory));
        match next.and_then(|e| e["oid"]["hex"].as_str()) {
            Some(hex) => oid = hex.to_owned(),
            // No recipe directory: no recipes.
            None => return Some(vec![]),
        }
    }
    None
}

/// What `environment.build` or `environment.admit` is about.
pub enum Target {
    /// A task, at its result head.
    Task { project: Uuid, head: String },
    /// A project's admitted recipes, at its main head (spec §6.4).
    Project(Uuid),
}
impl Target {
    fn project(&self) -> Uuid {
        match self {
            Target::Task { project, .. } | Target::Project(project) => *project,
        }
    }
}
/// aq-git could not list the recipes: dispatch again later.
#[derive(Debug)]
pub struct Unreachable;
type Answer = (&'static str, Value);
fn failed(recipe: &str, step: &str, log: &str) -> Answer {
    let output = json!({"recipe": cut(recipe, 256), "step": step, "log": cut(log, MAX_LOG)});
    ("failed", output)
}
fn cut(text: &str, bytes: usize) -> &str {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
/// The binding's name: the `<project>` of its environment ids.
async fn project_name(db: &mut PgConnection, project: Uuid) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar(
        "SELECT binding->>'name' FROM project_execution_bindings WHERE project_id=$1",
    )
    .bind(project)
    .fetch_optional(db)
    .await
    .map(Option::flatten)
}
/// `(environment id, recipe blob id)` of each environment's newest version.
async fn admitted(
    db: &mut PgConnection,
    project: Uuid,
) -> sqlx::Result<Vec<(String, Option<String>)>> {
    sqlx::query_as(
        "SELECT DISTINCT ON (environment_id) environment_id,recipe_blob_id FROM environment_versions \
         WHERE project_id=$1 ORDER BY environment_id,admitted_at DESC,\
         (CASE WHEN version ~ '^[0-9]{1,18}$' THEN version::bigint END) DESC NULLS LAST,version DESC",
    )
    .bind(project)
    .fetch_all(db)
    .await
}

/// `environment.build`, at dispatch and in its transaction: the operation's
/// frozen request (a queued builder job, or the answer it completes with
/// when no job is needed), or `Unreachable`.
pub async fn build(
    db: &mut PgConnection,
    ops: &dyn EnvironmentOps,
    target: &Target,
) -> sqlx::Result<Result<Value, Unreachable>> {
    let project = target.project();
    let answer = |(outcome, output): Answer| {
        Ok(Ok(
            json!({"project": project, "answer": {"outcome": outcome, "output": output}}),
        ))
    };
    let Some(name) = project_name(db, project).await? else {
        return answer(failed("", "subject", "the project has no binding"));
    };
    let prefix = format!("{name}.");
    let admitted = admitted(db, project).await?;
    let (commit, recipes) = match target {
        Target::Task { head, .. } => {
            let Some(at_head) = ops.recipes(project, head).await else {
                return Ok(Err(Unreachable));
            };
            let blob = |id: &str| admitted.iter().find(|(a, _)| a == id).map(|(_, b)| b);
            let mut changed: Vec<String> = (at_head.iter())
                .filter(|(n, b)| blob(&format!("{prefix}{n}")) != Some(&Some(b.clone())))
                .map(|(n, _)| n.clone())
                .collect();
            changed.sort();
            let removed = admitted.iter().any(|(id, _)| {
                !at_head
                    .iter()
                    .any(|(n, _)| id.strip_prefix(&prefix) == Some(n))
            });
            match (changed.is_empty(), removed) {
                (true, false) => return answer(("unchanged", json!({}))),
                // Only a recipe removed: nothing to build.
                (true, true) => return answer(("built", json!({"builds": []}))),
                _ => (head.clone(), changed),
            }
        }
        Target::Project(_) => {
            let head: Option<String> =
                sqlx::query_scalar("SELECT head FROM delivery_main_heads WHERE project_id=$1")
                    .bind(project)
                    .fetch_optional(&mut *db)
                    .await?;
            let Some(head) = head else {
                return answer(failed(
                    "",
                    "fetch",
                    "the project's main head is not known yet",
                ));
            };
            let names: Vec<String> = (admitted.iter())
                .filter_map(|(id, _)| id.strip_prefix(&prefix).map(str::to_owned))
                .collect();
            if names.is_empty() {
                return answer(("unchanged", json!({})));
            }
            (head, names)
        }
    };
    if recipes.len() > MAX_BUILDS {
        let log = format!(
            "{} recipes to build; one build takes at most {MAX_BUILDS}",
            recipes.len()
        );
        return answer(failed(&recipes[0], "parse", &log));
    }
    let Some(base) = store::base(&mut *db).await? else {
        return answer(failed(
            &recipes[0],
            "base",
            "no AQ base template is admitted",
        ));
    };
    let job = worker::NewJob {
        kind: "build",
        project_id: project,
        commit: &commit,
        recipes: &recipes,
        run: None,
        base_template: &base,
    };
    let job_id = worker::enqueue(&mut *db, &job).await?;
    Ok(Ok(json!({"project": project, "job_id": job_id})))
}

/// One step of a running `environment.build`: its completion once its job
/// completed, `None` while the job runs.
pub async fn build_step(pool: &PgPool, request: &Value) -> sqlx::Result<Option<Answer>> {
    if let Some(answer) = request.get("answer") {
        let outcome = match answer["outcome"].as_str() {
            Some("unchanged") => "unchanged",
            Some("built") => "built",
            _ => "failed",
        };
        return Ok(Some((outcome, answer["output"].clone())));
    }
    let job = request["job_id"]
        .as_str()
        .and_then(|j| Uuid::parse_str(j).ok());
    let row: Option<(String, Option<Value>)> =
        sqlx::query_as("SELECT state,result FROM environment_jobs WHERE job_id=$1")
            .bind(job)
            .fetch_optional(pool)
            .await?;
    Ok(match row {
        None => Some(failed("", "fetch", "the build's job record is gone")),
        Some((state, Some(result))) if state == "completed" => Some(summary(&result)),
        Some(_) => None,
    })
}
/// The catalog's answer for a job result: each Build's name, digest, recipe
/// hash and toolchain (each check command, and the first line of its output).
fn summary(result: &Value) -> Answer {
    if result["outcome"] != "built" {
        let f = &result["failure"];
        let text = |key: &str| f[key].as_str().unwrap_or_default().to_owned();
        return failed(&text("recipe"), &text("step"), &text("log"));
    }
    let builds = result["builds"]
        .as_array()
        .into_iter()
        .flatten()
        .take(MAX_BUILDS);
    let builds: Vec<Value> = builds
        .map(|b| {
            let text = |key: &str| cut(b[key].as_str().unwrap_or_default(), MAX_TEXT).to_owned();
            let toolchain = b["toolchain"].as_array().into_iter().flatten().take(MAX_BUILDS);
            let toolchains: Vec<Value> = toolchain
                .map(|t| {
                    let command = cut(t["command"].as_str().unwrap_or_default(), MAX_TEXT);
                    let output = t["output"].as_str().unwrap_or_default();
                    let first = output.lines().next().unwrap_or_default();
                    json!({"name": command, "version": cut(first, MAX_TEXT)})
                })
                .collect();
            json!({"name": text("name"), "digest": text("digest"), "recipe_hash": text("recipe_sha256"), "toolchains": toolchains})
        })
        .collect();
    ("built", json!({"builds": builds}))
}

/// Recipe names: `[a-z0-9][a-z0-9-]{0,63}`.
fn recipe_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.len() <= 64
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
/// The platform text of an admitted version (spec §5.5): what it provides
/// and the toolchain versions its checks recorded.
fn platform_text(id: &str, build: &Value) -> String {
    let provides: Vec<&str> = (build["provides"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .collect();
    let mut text = format!("Environment {id} provides: {}.", provides.join(", "));
    for t in build["toolchain"].as_array().into_iter().flatten() {
        let first = t["output"].as_str().unwrap_or_default().lines().next();
        let command = t["command"].as_str().unwrap_or_default();
        text.push_str(&format!("\n- `{command}`: {}", first.unwrap_or_default()));
    }
    cut(&text, 65536).to_owned()
}

/// One built recipe as admit reads it back.
struct Built {
    id: String,
    name: String,
    digest: String,
    base: String,
    blob: String,
    build: Value,
}
/// `environment.admit` of `request` (`{project, task, builds}`) as `actor`:
/// its completion, or `None` to try again in a later pass. Each build is
/// written as the next version of `<project>.<name>`, unless a version from
/// a later merge (or the same image) is there already; the binding then
/// allows each environment and takes the default recipe's.
pub async fn admit(
    pool: &PgPool,
    ops: &dyn EnvironmentOps,
    actor: Uuid,
    operation: Uuid,
    request: &Value,
) -> sqlx::Result<Option<Answer>> {
    let refused = |reason: &str, step: &str| {
        Ok(Some((
            "refused",
            json!({"reason": reason, "step": cut(step, 256)}),
        )))
    };
    let Some(project) = request["project"]
        .as_str()
        .and_then(|p| Uuid::parse_str(p).ok())
    else {
        return refused("unknown_project", "subject");
    };
    let task = request["task"]
        .as_str()
        .and_then(|t| Uuid::parse_str(t).ok());
    let mut conn = pool.acquire().await?;
    let Some(name) = project_name(&mut conn, project).await? else {
        return refused("unknown_project", "binding");
    };
    let mut builds: Vec<Built> = vec![];
    for asked in request["builds"].as_array().into_iter().flatten() {
        let (recipe, digest) = (asked["name"].as_str(), asked["digest"].as_str());
        let row: Option<(String, String, String, Value)> = sqlx::query_as(
            "SELECT name,commit,base_template,build FROM environment_builds WHERE project_id=$1 AND digest=$2",
        )
        .bind(project)
        .bind(digest)
        .fetch_optional(&mut *conn)
        .await?;
        let recipe = recipe.unwrap_or_default();
        let Some((built, commit, base, build)) = row.filter(|r| r.0 == recipe) else {
            return refused("unknown_build", recipe);
        };
        if !recipe_name(&built) {
            return refused("unknown_build", recipe);
        }
        // The recipe's blob at the commit it was built from.
        let Some(at) = ops.recipes(project, &commit).await else {
            return refused("unreachable", "tree");
        };
        let Some((_, blob)) = at.into_iter().find(|(n, _)| *n == built) else {
            return refused("unknown_build", recipe);
        };
        builds.push(Built {
            id: format!("{name}.{built}"),
            digest: digest.unwrap_or_default().to_owned(),
            base: base
                .split_once('@')
                .map(|(_, d)| d.to_owned())
                .unwrap_or_default(),
            name: built,
            blob,
            build,
        });
    }
    let defaults: Vec<&Built> = builds
        .iter()
        .filter(|b| b.build["default"] == true)
        .collect();
    if defaults.len() > 1 {
        // The catalog's refusals have no "ambiguous"; the builds are what is wrong.
        return refused("unknown_build", "default");
    }
    // A task's versions are ordered by when the server saw it merge.
    let merged: Option<i64> = match task {
        Some(task) => {
            let merged: Option<i64> = sqlx::query_scalar(
                "SELECT max(observed_at_ms) FROM delivery_merge_evidence WHERE task_id=$1 AND project_id=$2",
            )
            .bind(task)
            .bind(project)
            .fetch_one(&mut *conn)
            .await?;
            if merged.is_none() {
                return refused("unknown_task", "merge");
            }
            merged
        }
        None => None,
    };
    let (limits, _) = aq_role_settings::environment_limits(pool)
        .await
        .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    let mut tx = pool.begin().await?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtext('aq.environment.admit'),hashtext($1::text))",
    )
    .bind(project)
    .execute(&mut *tx)
    .await?;
    let mut default = None;
    for b in &builds {
        write(&mut tx, project, merged, b, limits.keep_versions).await?;
        let newest: Option<String> = sqlx::query_scalar(
            "SELECT template_digest FROM environment_versions WHERE environment_id=$1 ORDER BY admitted_at DESC,\
             (CASE WHEN version ~ '^[0-9]{1,18}$' THEN version::bigint END) DESC NULLS LAST LIMIT 1",
        )
        .bind(&b.id)
        .fetch_optional(&mut *tx)
        .await?;
        // Only the version that stands takes the default.
        if b.build["default"] == true && newest.as_deref() == Some(b.digest.as_str()) {
            default = Some(b.id.clone());
        }
    }
    tx.commit().await?;
    let ids: Vec<String> = builds.iter().map(|b| b.id.clone()).collect();
    let digests: Vec<&str> = builds.iter().map(|b| b.digest.as_str()).collect();
    if !ids.is_empty() {
        match ops
            .bind(actor, project, &ids, default.as_deref(), operation)
            .await
        {
            Bound::Done => {}
            Bound::Retry => return Ok(None),
            Bound::Refused => {
                // Nothing is half-registered: the versions go again.
                sqlx::query("DELETE FROM environment_versions WHERE project_id=$1 AND template_digest=ANY($2)")
                    .bind(project)
                    .bind(&digests)
                    .execute(pool)
                    .await?;
                return refused("unknown_project", "binding");
            }
        }
    }
    sqlx::query("DELETE FROM environment_builds WHERE project_id=$1 AND digest=ANY($2)")
        .bind(project)
        .bind(&digests)
        .execute(pool)
        .await?;
    Ok(Some(("admitted", json!({}))))
}
/// Writes `b` as the next version of its environment unless a version of
/// the same image, or from a later merge, is there; then keeps the newest
/// `keep` versions, and every version a live attempt runs.
async fn write(
    tx: &mut PgConnection,
    project: Uuid,
    merged: Option<i64>,
    b: &Built,
    keep: u64,
) -> sqlx::Result<()> {
    let newest: Option<(Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT recipe_blob_id,merged_at_ms FROM environment_versions WHERE environment_id=$1 ORDER BY admitted_at DESC,\
         (CASE WHEN version ~ '^[0-9]{1,18}$' THEN version::bigint END) DESC NULLS LAST LIMIT 1",
    )
    .bind(&b.id)
    .fetch_optional(&mut *tx)
    .await?;
    let merged = match merged {
        Some(merged) => merged,
        // A rebuild on a new base (spec §6.4) stands in for the newest
        // version, and only while that is still the recipe it rebuilt.
        None => match newest {
            Some((Some(blob), merged)) if blob == b.blob => merged.unwrap_or(0),
            _ => return Ok(()),
        },
    };
    let inserted = sqlx::query(
        "INSERT INTO environment_versions(environment_id,version,project_id,template_digest,image,provides,\
         platform_text,toolchain,warm,harnesses,recipe_sha256,base_digest,recipe_blob_id,merged_at_ms) \
         SELECT $1,(COALESCE(max(CASE WHEN version ~ '^[0-9]{1,18}$' THEN version::bigint END),0)+1)::text,\
         $2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13 FROM environment_versions WHERE environment_id=$1 \
         HAVING NOT (bool_or(template_digest=$3 OR merged_at_ms>$13) IS TRUE)",
    )
    .bind(&b.id)
    .bind(project)
    .bind(&b.digest)
    .bind(b.build["image"].as_str())
    .bind(&b.build["provides"])
    .bind(platform_text(&b.id, &b.build))
    .bind(&b.build["toolchain"])
    .bind(&b.build["warm"])
    .bind(&b.build["harnesses"])
    .bind(b.build["recipe_sha256"].as_str())
    .bind(Some(&b.base).filter(|d| !d.is_empty()))
    .bind(&b.blob)
    .bind(merged)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 1 {
        tracing::info!(environment = %b.id, recipe = %b.name, digest = %b.digest, "environment version admitted");
    }
    prune(tx, &b.id, keep).await
}
/// Rolling records: the newest `keep` versions of `environment`, and any
/// version an attempt that holds its task still runs (its synthesized
/// inventory entry needs the row).
pub async fn prune(tx: &mut PgConnection, environment: &str, keep: u64) -> sqlx::Result<()> {
    sqlx::query(
        "DELETE FROM environment_versions v WHERE environment_id=$1 AND version IN (SELECT version FROM (\
         SELECT version,row_number() OVER (ORDER BY admitted_at DESC,\
         (CASE WHEN version ~ '^[0-9]{1,18}$' THEN version::bigint END) DESC NULLS LAST,version DESC) AS n \
         FROM environment_versions WHERE environment_id=$1) o WHERE n>$2) \
         AND NOT EXISTS (SELECT 1 FROM execution_attempts a JOIN profiled_launches l USING(attempt_id) \
         WHERE a.holds_task AND l.launch_manifest->'harness'->'environment'->>'id'=v.environment_id \
         AND l.launch_manifest->'harness'->'environment'->>'version'=v.version)",
    )
    .bind(environment)
    .bind(i64::try_from(keep).unwrap_or(i64::MAX))
    .execute(tx)
    .await?;
    Ok(())
}

/// The AQ base template the boot config names (spec §6.4).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Base {
    /// The base's environment id, for example `aq-base`.
    pub id: String,
    /// `repo@sha256:<hex>`.
    pub image: String,
    /// The harness versions its runtime bundle holds (`[{harness, version}]`).
    #[serde(default)]
    pub harnesses: Vec<Value>,
}
/// At startup: records the boot base as the next base version when its
/// digest is not the newest base's. Whether it recorded one.
pub async fn record_base(pool: &PgPool, base: &Base) -> sqlx::Result<bool> {
    let digest = base
        .image
        .split_once('@')
        .map(|(_, d)| d)
        .unwrap_or_default();
    let newest = store::base(pool).await?;
    if newest
        .as_deref()
        .and_then(|b| b.split_once('@'))
        .map(|(_, d)| d)
        == Some(digest)
    {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO environment_versions(environment_id,version,template_digest,image,harnesses) \
         SELECT $1,(COALESCE(max(CASE WHEN version ~ '^[0-9]{1,18}$' THEN version::bigint END),0)+1)::text,$2,$3,$4 \
         FROM environment_versions WHERE environment_id=$1",
    )
    .bind(&base.id)
    .bind(digest)
    .bind(&base.image)
    .bind(json!(base.harnesses))
    .execute(pool)
    .await?;
    tracing::info!(base = %base.image, "AQ base template admitted from the boot config");
    Ok(true)
}
/// `(project, base digest)`: each project (`only` one, when given) whose
/// newest version of some environment is built on another base than the
/// newest, and that was not told of that base yet.
pub async fn base_changes(
    db: &mut PgConnection,
    only: Option<Uuid>,
    limit: u32,
) -> sqlx::Result<Vec<(Uuid, String)>> {
    sqlx::query_as(
        "WITH base AS (SELECT template_digest AS digest FROM environment_versions WHERE project_id IS NULL \
         ORDER BY admitted_at DESC,(CASE WHEN version ~ '^[0-9]{1,18}$' THEN version::bigint END) DESC NULLS LAST LIMIT 1), \
         newest AS (SELECT DISTINCT ON (environment_id) project_id,base_digest FROM environment_versions \
         WHERE project_id IS NOT NULL ORDER BY environment_id,admitted_at DESC,\
         (CASE WHEN version ~ '^[0-9]{1,18}$' THEN version::bigint END) DESC NULLS LAST) \
         SELECT DISTINCT n.project_id,b.digest FROM newest n,base b \
         WHERE n.base_digest IS DISTINCT FROM b.digest AND ($1::uuid IS NULL OR n.project_id=$1) \
         AND NOT EXISTS (SELECT 1 FROM environment_base_changes c WHERE c.project_id=n.project_id AND c.base_digest=b.digest) \
         ORDER BY n.project_id LIMIT $2",
    )
    .bind(only)
    .bind(i64::from(limit))
    .fetch_all(db)
    .await
}
/// `project` was told of the base `digest` (one row per project).
pub async fn told(db: &mut PgConnection, project: Uuid, digest: &str) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO environment_base_changes(project_id,base_digest) VALUES($1,$2) \
         ON CONFLICT(project_id) DO UPDATE SET base_digest=excluded.base_digest",
    )
    .bind(project)
    .bind(digest)
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_names_each_check_and_the_first_line_of_its_output() {
        let result = json!({"outcome": "built", "builds": [{"name": "web", "digest": "sha256:aa",
            "recipe_sha256": "bb", "toolchain": [{"command": "node --version", "output": "v24.1.0\nmore"}]}]});
        assert_eq!(
            summary(&result),
            (
                "built",
                json!({"builds": [{"name": "web", "digest": "sha256:aa", "recipe_hash": "bb",
                "toolchains": [{"name": "node --version", "version": "v24.1.0"}]}]})
            )
        );
        let long = "é".repeat(5000);
        let failure = json!({"outcome": "failed", "failure": {"recipe": "web", "step": "check", "log": long}});
        let (outcome, output) = summary(&failure);
        assert_eq!((outcome, &output["step"]), ("failed", &json!("check")));
        assert!(output["log"].as_str().unwrap().len() <= MAX_LOG);
        assert!(
            recipe_name("web-2") && !recipe_name("Web") && !recipe_name("a.b") && !recipe_name("")
        );
    }
}

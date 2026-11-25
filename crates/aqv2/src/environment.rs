//! Spec 0017 §6.2 (feature `project_environments`): `aq environment try NAME
//! [--run CMD]` builds a recipe of the attempt's project on a builder and runs
//! one command in the image. The request goes to the agent route itself, not
//! the command surface: the node's channel proxy commits and pushes the
//! workspace for that route and adds the pushed `commit` to the body. The CLI
//! then polls the job until it completes and prints the builder's Job result;
//! a failed build or a nonzero `run.exit` is `environment_try_failed` (exit 1).
use crate::{Ctx, Mode, REFUSED, Result, client, error, program};
use clap::Subcommand;
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use uuid::Uuid;

const TRY: &str = "/v1/agent/environments/try";

#[derive(Subcommand)]
pub enum Environment {
    /// Build `.aq/environments/NAME.md` from your workspace on a builder and
    /// run one command in the result. Your node pushes the workspace to your
    /// task's branch first. Prints the job's result (outcome, failure, run).
    Try {
        /// The recipe name.
        name: String,
        /// One shell command to run in the built image.
        #[arg(long)]
        run: Option<String>,
        /// Seconds to wait for the result in total.
        #[arg(long, default_value_t = 3600, value_parser = clap::value_parser!(u64).range(1..=86400))]
        timeout: u64,
        /// Seconds between checks.
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=60))]
        interval: u64,
    },
    #[command(external_subcommand)]
    Other(Vec<String>),
}

/// A refused try, by its code. The server's message is never echoed.
fn refused(body: &Value) -> Option<Value> {
    let code = body["code"].as_str()?;
    let message = match code {
        "workspace_not_pushed" => {
            "Your workspace was not pushed with the request: this node's channel does not push it for `environment try`"
        }
        "try_concurrency_reached" => {
            "Your project already runs as many try jobs as environment_limits.try_concurrency allows; retry when one completes"
        }
        "no_base_template" => {
            "No AQ base template is admitted yet, so there is nothing to build on"
        }
        "invalid_request" => "The server refused the request: check the recipe name and --run",
        c if REFUSED.contains(&c) => {
            "Refused: trying an environment needs environment.try for your project"
        }
        _ => return None,
    };
    Some(error(code, message))
}

pub async fn environment(ctx: &Ctx, c: &Environment) -> Result<Value> {
    let (name, run, timeout, interval) = match c {
        Environment::Try {
            name,
            run,
            timeout,
            interval,
        } => (name, run, *timeout, *interval),
        Environment::Other(rest) => {
            let tokens: Vec<String> = std::iter::once("environment".to_owned())
                .chain(rest.iter().cloned())
                .collect();
            return crate::surface::external(ctx, &tokens).await;
        }
    };
    if ctx.mode != Mode::Channel {
        return Err(error(
            "unsupported",
            "`environment try` runs inside a task sandbox, through its channel",
        ));
    }
    let mut body = json!({"api_version": 1, "name": name});
    if let Some(run) = run {
        body["run"] = json!(run);
    }
    let raw = (ctx.http.post_json_response(TRY, &body).await).map_err(client::http_error)?;
    if !(200..300).contains(&raw.status) {
        return Err(refused(&raw.body).unwrap_or_else(|| {
            client::response(raw)
                .err()
                .unwrap_or_else(|| error("invalid_response", "Unexpected response"))
        }));
    }
    let job: Uuid = (raw.body["job_id"].as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(|| error("invalid_response", "The response names no job"))?;
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        let value = client::read(&ctx.http, &format!("{TRY}/{job}"), &[]).await?;
        if value["state"] == "completed" {
            // A failed build or a failing command exits nonzero, with the result.
            let result = &value["result"];
            let exit = result["run"]["exit"].as_i64();
            if result["outcome"] == "failed" || exit.is_some_and(|e| e != 0) {
                let mut e = error(
                    "environment_try_failed",
                    "The build failed or the command exited nonzero; the job's result is printed",
                );
                e["job"] = value;
                return Err(e);
            }
            return Ok(value);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            let mut e = error(
                "wait_timeout",
                &format!(
                    "Job {job} did not complete within {timeout}s; `{} environment job show --job-id {job}` reads it again",
                    program()
                ),
            );
            e["job"] = value;
            return Err(e);
        }
        tokio::time::sleep(left.min(Duration::from_secs(interval))).await;
    }
}

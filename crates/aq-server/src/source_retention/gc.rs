//! SBX24: release settled task checkpoints through aq-git's guarded API.
//! Historical import receipts are retained. Never delete aq-git's files here.
use super::*;
use chrono::{DateTime, Utc};
use tokio::sync::watch;

// A receipt of execution alone is not task acceptance. These are the actual
// task-completion and publication owners. Use the latest settlement timestamp
// conservatively if a task has more than one owner fact.
const SETTLED: &str = "
 WITH settlements AS (
  SELECT a.task_id,c.created_at_ms AS settled_at FROM profiled_task_completions c
  JOIN execution_attempts a USING(attempt_id) WHERE c.state='confirmed'
  UNION ALL
  SELECT a.task_id,c.updated_at_ms FROM profiled_auto_settlements c
  JOIN execution_attempts a USING(attempt_id) WHERE c.disposition='confirmed'
  UNION ALL
  SELECT a.task_id,d.recorded_at_ms FROM profiled_delivery_evidence d
  JOIN execution_attempts a USING(attempt_id) WHERE d.state='published'
 ) SELECT task_id,max(settled_at) AS settled_at FROM settlements
 WHERE ($1::uuid IS NULL OR task_id>$1) GROUP BY task_id ORDER BY task_id LIMIT 8";

#[derive(Default)]
pub struct Sweep {
    after: Option<Uuid>,
}

/// A held claim includes stopped attempts whose source/effects remain uncertain
/// or preserved. Use the hot authority for adopted native attempts, never their
/// historical compatibility projection. Unknown shapes are protected.
fn protected(held: bool, kind: &str, state: &Value) -> bool {
    if held {
        return true;
    }
    match kind {
        "legacy_fixture" => {
            state["status"] != "finished"
                || state["stopped"] != true
                || state["disposition"]["retention_release"] != true
                || state["report"]["pending_operation_ids"] != json!([])
        }
        "profiled_v1" => {
            !matches!(
                state["status"].as_str(),
                Some("finished" | "declined" | "never_granted")
            ) || (state["status"] != "never_granted"
                && !matches!(
                    state["stop"]["kind"].as_str(),
                    Some("stopped" | "not_started")
                ))
        }
        _ => true,
    }
}

fn release_reason(
    settled_at: i64,
    now: DateTime<Utc>,
    rank: usize,
    config: &SourceRetentionConfig,
) -> Option<&'static str> {
    if now.timestamp_millis().saturating_sub(settled_at) >= config.max_age as i64 * 1000 {
        Some("source_retention.max_age")
    } else if rank >= config.keep_per_task {
        Some("source_retention.keep_per_task")
    } else {
        None
    }
}

impl Sweep {
    /// One bounded page per tick. Failed tasks advance the cursor too, so an
    /// unavailable handle cannot permanently starve other settled tasks.
    pub async fn tick(&mut self, state: &AppState) -> Result<usize> {
        let rt = runtime(state)?;
        let tasks: Vec<(Uuid, i64)> = sqlx::query_as(SETTLED)
            .bind(self.after)
            .fetch_all(&state.store.pool)
            .await
            .map_err(internal)?;
        if tasks.is_empty() {
            self.after = None;
        }
        let mut released = 0;
        for (task, settled_at) in tasks {
            self.after = Some(task);
            match collect_task(state, &rt, task, settled_at).await {
                Ok(count) => released += count,
                Err(error) => tracing::warn!(%task, ?error, "source retention GC deferred"),
            }
        }
        Ok(released)
    }
}

async fn task_protected(state: &AppState, task: Uuid) -> Result<bool> {
    let rows = sqlx::query("SELECT a.holds_task,a.kind,CASE WHEN a.kind='profiled_v1' THEN h.state ELSE a.state END AS authority FROM execution_attempts a LEFT JOIN profiled_authority_hot h USING(attempt_id) WHERE a.task_id=$1")
        .bind(task).fetch_all(&state.store.pool).await.map_err(internal)?;
    Ok(rows.is_empty()
        || rows.iter().any(|r| {
            protected(
                r.get("holds_task"),
                r.get::<String, _>("kind").as_str(),
                &r.get::<Option<Value>, _>("authority")
                    .unwrap_or(Value::Null),
            )
        }))
}

async fn collect_task(
    state: &AppState,
    rt: &Runtime,
    task: Uuid,
    settled_at: i64,
) -> Result<usize> {
    if task_protected(state, task).await? {
        return Ok(0);
    }
    // Count every original retained checkpoint, including already released old
    // checkpoints. A retry must not rotate the newest keep_per_task window.
    let rows = sqlx::query("SELECT t.transfer_id,t.repository_id,t.artifact_id,t.manifest,r.request,r.released_at FROM source_transfers t JOIN execution_attempts a USING(attempt_id) LEFT JOIN source_retention_releases r USING(transfer_id) WHERE a.task_id=$1 AND t.state='retained' ORDER BY t.created_at DESC,t.transfer_id DESC")
        .bind(task).fetch_all(&state.store.pool).await.map_err(internal)?;
    let mut count = 0;
    for (rank, row) in rows.iter().enumerate() {
        let Some(reason) = release_reason(settled_at, (state.clock)(), rank, &rt.config) else {
            continue;
        };
        if row.get::<Option<DateTime<Utc>>, _>("released_at").is_some() {
            continue;
        }
        let transfer: Uuid = row.get("transfer_id");
        let request = if let Some(request) = row.get::<Option<Value>, _>("request") {
            request
        } else {
            let manifest: TransferManifest =
                serde_json::from_value(row.get("manifest")).map_err(internal)?;
            let handle: Uuid = row.get("artifact_id");
            let repository: Uuid = row.get("repository_id");
            if manifest.artifact_id != handle || manifest.source.repository_id != repository {
                return Err(conflict("retained source identity mismatch"));
            }
            let inspect = json!({"protocol_version":1,"request_id":Uuid::new_v4(),"operation":"retention.inspect","repository_id":repository,"expected_binding_generation":manifest.source.binding_generation,"params":{"handle_id":handle,"limit":1},"deadline_ms":5000});
            let record = exchange(rt, &inspect).await?;
            let request = release_request(&manifest, &record, reason)?;
            // ON CONFLICT preserves a previous sweep's exact request, including
            // its operation UUID, even after a lost mutation response.
            sqlx::query("INSERT INTO source_retention_releases(transfer_id,operation_id,handle_generation,reason,request) VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING")
                .bind(transfer).bind(serde_json::from_value::<Uuid>(request["operation_id"].clone()).map_err(internal)?)
                .bind(record["generation"].as_i64().ok_or_else(|| conflict("invalid handle generation"))?)
                .bind(reason).bind(&request).execute(&state.store.pool).await.map_err(internal)?;
            sqlx::query_scalar("SELECT request FROM source_retention_releases WHERE transfer_id=$1")
                .bind(transfer)
                .fetch_one(&state.store.pool)
                .await
                .map_err(internal)?
        };
        // Recheck authority after inspection/intent I/O and before release.
        if task_protected(state, task).await? {
            break;
        }
        let response = exchange(rt, &request).await?;
        verify_release(&request, &response)?;
        sqlx::query("UPDATE source_retention_releases SET response=$2,released_at=now() WHERE transfer_id=$1 AND released_at IS NULL")
            .bind(transfer).bind(&response).execute(&state.store.pool).await.map_err(internal)?;
        tracing::info!(%task, %transfer, handle=%request["params"]["handle_id"], generation=%request["params"]["expected_generation"], reason=%request["params"]["reason"], "source retention released");
        count += 1;
    }
    Ok(count)
}

fn release_request(manifest: &TransferManifest, record: &Value, reason: &str) -> Result<Value> {
    if record["version"] != 1
        || record["id"] != json!(manifest.artifact_id)
        || record["repository_id"] != json!(manifest.source.repository_id)
        || record["binding_generation"] != manifest.source.binding_generation
        || record["generation"].as_i64().is_none_or(|g| g <= 0)
        || record["resource"] != "transfer"
        || record["state"] != "active"
        || record["refs"]
            != json!([{"name":format!("refs/aq-git/transfers/{}",manifest.artifact_id),"oid":manifest.checkpoint.hex}])
    {
        return Err(conflict("source retention inspection mismatch"));
    }
    let operation = Uuid::new_v4();
    Ok(
        json!({"protocol_version":1,"request_id":operation,"operation_id":operation,"operation":"retention.release","repository_id":manifest.source.repository_id,"expected_binding_generation":manifest.source.binding_generation,"params":{"handle_id":manifest.artifact_id,"expected_generation":record["generation"],"refs":record["refs"],"reason":reason},"deadline_ms":5000}),
    )
}

fn verify_release(request: &Value, response: &Value) -> Result<()> {
    if response["version"] != 1
        || response["id"] != request["params"]["handle_id"]
        || response["repository_id"] != request["repository_id"]
        || response["binding_generation"] != request["expected_binding_generation"]
        || response["generation"]
            .as_u64()
            .zip(request["params"]["expected_generation"].as_u64())
            .is_none_or(|(actual, expected)| actual <= expected)
        || response["state"] != "released"
        || response["refs"] != request["params"]["refs"]
        || response["release_operation"] != request["operation_id"]
        || response["release_reason"] != request["params"]["reason"]
    {
        return Err(conflict("source retention release not confirmed"));
    }
    Ok(())
}

async fn exchange(rt: &Runtime, request: &Value) -> Result<Value> {
    let _permit = rt
        .bulk
        .try_acquire()
        .map_err(|_| conflict("source retention busy"))?;
    let call = async {
        let socket = tokio::net::UnixStream::connect(&rt.config.git_socket)
            .await
            .map_err(internal)?;
        let mut stream = BufReader::new(socket);
        let mut bytes = serde_json::to_vec(request).map_err(internal)?;
        bytes.push(b'\n');
        stream.get_mut().write_all(&bytes).await.map_err(internal)?;
        let mut frame = Vec::new();
        stream
            .take(32769)
            .read_until(b'\n', &mut frame)
            .await
            .map_err(internal)?;
        if frame.len() > 32768 || frame.last() != Some(&b'\n') {
            return Err(conflict("invalid aq-git GC frame"));
        }
        let reply: Value = serde_json::from_slice(&frame).map_err(internal)?;
        if reply["protocol_version"] != 1
            || reply["request_id"] != request["request_id"]
            || reply["ok"] != true
            || reply.get("error").is_some()
            || (request.get("operation_id").is_some()
                && (reply["operation_id"] != request["operation_id"]
                    || reply["operation_state"] != "complete"))
        {
            return Err(conflict("aq-git GC outcome unconfirmed"));
        }
        Ok(reply["result"].clone())
    };
    tokio::time::timeout(Duration::from_secs(5), call)
        .await
        .map_err(internal)?
}

pub async fn run(state: AppState, mut stop: watch::Receiver<bool>) {
    if state.source_retention.is_none() {
        return;
    }
    let mut sweep = Sweep::default();
    let mut ticks = tokio::time::interval(Duration::from_secs(60));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|value| *value) => return,
            _ = ticks.tick() => {}
        }
        tokio::select! {
            biased;
            _ = stop.wait_for(|value| *value) => return,
            result = sweep.tick(&state) => {
                if let Err(error) = result { tracing::warn!(?error, "source retention GC sweep deferred"); }
            }
        }
    }
}

#[cfg(test)]
mod tests;

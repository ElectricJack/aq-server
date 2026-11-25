use super::*;
use aq_machine_control::{self as control, management, slots};
use chrono::{DateTime, Utc};
use tokio::sync::watch;

const RETENTION: i64 = 7 * DAY;
const MAX_NODES: usize = 256;
const MAX_PROJECTS: usize = 16;

pub(crate) fn spawn(state: AppState, mut shutdown: watch::Receiver<bool>) {
    if std::env::var("AQ_PERFORMANCE_SAMPLING").as_deref() == Ok("off") {
        return;
    }
    tokio::spawn(async move {
        tokio::select! {
            _ = shutdown.wait_for(|stop| *stop) => (),
            _ = run(&state) => (),
        }
    });
}

async fn run(state: &AppState) {
    loop {
        let now = (state.clock)();
        match tokio::time::timeout(Duration::from_secs(45), tick(state, now)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(%error, "performance minute could not be sampled");
            }
            Err(_) => tracing::warn!("performance minute sampling timed out"),
        }
        let at = (state.clock)().timestamp_millis();
        let delay = MINUTE - at.rem_euclid(MINUTE);
        tokio::time::sleep(Duration::from_millis(delay as u64)).await;
    }
}

async fn tick(state: &AppState, now: DateTime<Utc>) -> Result<()> {
    let pool = &state.store.pool;
    let minute = now.timestamp_millis().div_euclid(MINUTE) * MINUTE;
    // Avoid work on a replay or when another server has already committed this tick.
    if sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM performance_fleet_minutes WHERE minute_ms=$1)",
    )
    .bind(minute)
    .fetch_one(pool)
    .await?
    {
        return Ok(());
    }
    let (inventory, ready) = tokio::join!(inventory(state, now), ready(state));
    let (inventory_complete, samples) = inventory?;
    let observed = (state.clock)().timestamp_millis();
    if observed.div_euclid(MINUTE) * MINUTE != minute {
        return Ok(());
    }
    store_tick(pool, minute, observed, inventory_complete, samples, ready).await
}

async fn store_tick(
    pool: &PgPool,
    minute: i64,
    observed: i64,
    inventory_complete: bool,
    samples: BTreeMap<String, Machine>,
    ready: Ready,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    // First successful collector wins. The foreign key and transaction keep
    // concurrent collectors, cancellation and restart from publishing half a tick.
    let inserted = sqlx::query(
        "INSERT INTO performance_fleet_minutes
         (minute_ms,observed_at_ms,inventory_complete,ready_count,ready_accuracy)
         VALUES ($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING",
    )
    .bind(minute)
    .bind(observed)
    .bind(inventory_complete)
    .bind(ready.count)
    .bind(ready.accuracy)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 1 {
        if !samples.is_empty() {
            let mut query = sqlx::QueryBuilder::new(
                "INSERT INTO performance_machine_minutes
                 (minute_ms,machine_id,label,state,enabled_slots,occupied_slots,free_slots,enabled_occupied_slots) ",
            );
            query.push_values(samples.into_values(), |mut values, sample| {
                values
                    .push_bind(minute)
                    .push_bind(sample.machine_id)
                    .push_bind(sample.label)
                    .push_bind(sample.state)
                    .push_bind(sample.enabled_slots)
                    .push_bind(sample.occupied_slots)
                    .push_bind(sample.free_slots)
                    .push_bind(sample.enabled_occupied_slots);
            });
            query.build().execute(&mut *tx).await?;
        }
        sqlx::query(
            "DELETE FROM performance_fleet_minutes WHERE minute_ms IN (
               SELECT minute_ms FROM performance_fleet_minutes WHERE minute_ms <= $1
               ORDER BY minute_ms LIMIT 120
             )",
        )
        .bind(minute - RETENTION)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

struct Ready {
    count: Option<i64>,
    accuracy: &'static str,
}

async fn ready(state: &AppState) -> Ready {
    let (Some(config), Some(graph)) = (&state.config.profiled_execution, &state.agent_access)
    else {
        return Ready {
            count: None,
            accuracy: "unknown",
        };
    };
    let projects = config.enabled_projects();
    let mut complete = projects.len() <= MAX_PROJECTS;
    let mut read = false;
    let mut workspaces = BTreeSet::new();
    let mut tasks = BTreeSet::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    for project in projects.into_iter().take(MAX_PROJECTS) {
        let page = tokio::time::timeout_at(deadline, async {
            let record = crate::project_bindings::current(state, project)
                .await
                .ok()?;
            let workspace = record.binding.graph_workspace_id;
            if !record.binding.enabled || !workspaces.insert(workspace) {
                return Some(None);
            }
            let page = graph
                .scheduling_ready_for_project(
                    &record.binding,
                    64,
                    None,
                    state.clock.as_ref(),
                    &state.store.pool,
                )
                .await
                .ok()?;
            Some(Some(page))
        })
        .await;
        match page {
            Ok(Some(Some(page))) => {
                read = true;
                complete &= page.next_cursor.is_none();
                tasks.extend(page.items.into_iter().map(|item| item.revision.task_id));
            }
            Ok(Some(None)) => {}
            Ok(None) => complete = false,
            Err(_) => {
                complete = false;
                break;
            }
        }
    }
    Ready {
        count: (read || complete).then_some(tasks.len() as i64),
        accuracy: if complete {
            "exact"
        } else if read {
            "lower_bound"
        } else {
            "unknown"
        },
    }
}

struct NodeFact {
    id: Uuid,
    name: String,
    machine_key: Option<String>,
    last_seen_at: Option<DateTime<Utc>>,
    reported_key: Option<String>,
    reported_label: Option<String>,
}

#[derive(Default)]
struct Machine {
    machine_id: String,
    label: String,
    state: &'static str,
    enabled_slots: i64,
    occupied_slots: i64,
    free_slots: i64,
    enabled_occupied_slots: i64,
}

impl Machine {
    fn record(&mut self, state: &'static str, view: slots::SlotView) {
        self.state = match self.state {
            "" => state,
            current if current == state => current,
            _ => "partial",
        };
        self.occupied_slots += i64::from(view.occupied);
        if state == "online" {
            self.enabled_slots += i64::from(view.total);
            self.enabled_occupied_slots += i64::from(view.occupied.min(view.total));
            self.free_slots += i64::from(view.idle);
        }
    }

    fn cap(&mut self, max: u32) {
        self.enabled_slots = self.enabled_slots.min(i64::from(max));
        // Running attempts can outlive a lowered limit. Preserve their
        // occupancy, but count at most the enabled capacity in utilization.
        self.enabled_occupied_slots = self.enabled_occupied_slots.min(self.enabled_slots);
        self.free_slots = self
            .free_slots
            .min((self.enabled_slots - self.occupied_slots).max(0));
    }
}

async fn inventory(
    state: &AppState,
    now: DateTime<Utc>,
) -> Result<(bool, BTreeMap<String, Machine>)> {
    let mut tx = read_snapshot(&state.store.pool).await?;
    let nodes = sqlx::query(
        "SELECT n.id,n.name,n.machine_key,n.last_seen_at,
                r.report#>>'{machine,key}' AS reported_key,r.report#>>'{machine,label}' AS reported_label
         FROM nodes n LEFT JOIN node_machine_reports r ON r.node_id=n.id
         LEFT JOIN worker_settings s ON s.kind='node' AND s.id=n.id::text
         WHERE n.revoked_at IS NULL AND NOT COALESCE(s.removed,FALSE)
         ORDER BY n.name,n.id LIMIT $1",
    )
    .bind((MAX_NODES + 1) as i64)
    .try_map(|row| {
        Ok(NodeFact {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            machine_key: row.try_get("machine_key")?,
            last_seen_at: row.try_get("last_seen_at")?,
            reported_key: row.try_get("reported_key")?,
            reported_label: row.try_get("reported_label")?,
        })
    })
    .fetch_all(&mut *tx)
    .await?;
    if nodes.len() > MAX_NODES {
        // The omitted nodes could belong to a sampled machine.
        return Ok((false, BTreeMap::new()));
    }
    let ids = nodes.iter().map(|node| node.id).collect::<Vec<_>>();
    let services = aq_swarm::service::states(&mut tx, &ids).await?;
    let owner_paused =
        control::owner_availability(&mut tx).await?.state == control::Availability::Off;
    let settings = management::settings(&mut tx).await?;
    let held = nodes
        .iter()
        .filter_map(|node| node.machine_key.as_deref())
        .collect();
    let mut machines = BTreeMap::new();
    for node in &nodes {
        let key = aq_swarm::assemble::machine_key(
            node.id,
            node.machine_key.as_deref(),
            node.reported_key.as_deref(),
            &held,
        );
        let setting = settings.iter().find(|s| s.kind == "machine" && s.id == key);
        let label = setting
            .and_then(|s| s.label.clone())
            .or_else(|| node.reported_label.clone())
            .unwrap_or_else(|| {
                if key.starts_with("node:") {
                    node.name.clone()
                } else {
                    key.clone()
                }
            });
        let machine = machines.entry(key.clone()).or_insert_with(|| Machine {
            machine_id: key,
            label,
            ..Machine::default()
        });
        let paused = owner_paused
            || services.get(&node.id).is_some_and(|service| {
                use aq_dashboard_contract::ServiceState;
                matches!(
                    service.state,
                    ServiceState::Stopped | ServiceState::Draining
                )
            });
        let status = status(
            paused,
            node.last_seen_at,
            now,
            Duration::from_secs(state.config.workers.offline_after_secs),
        );
        machine.record(status, slots::view(&mut tx, node.id).await?);
    }
    for setting in settings.iter().filter(|s| s.kind == "machine") {
        if let (Some(max), Some(machine)) = (setting.max_agents, machines.get_mut(&setting.id)) {
            machine.cap(max);
        }
    }
    tx.commit().await?;
    Ok((true, machines))
}

fn status(
    paused: bool,
    seen: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    ttl: Duration,
) -> &'static str {
    if paused {
        "paused"
    } else if let Some(seen) = seen {
        if (now - seen).to_std().is_ok_and(|age| age > ttl) {
            "stale"
        } else {
            "online"
        }
    } else {
        "offline"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn minute_commit_is_atomic_across_failure_concurrency_and_replay() {
        let test = crate::test_support::TestDb::create().await;
        let store = test.store().await;
        let pool = &store.pool;
        let minute = RETENTION + 2 * MINUTE;
        let samples = |state, id: &str| {
            BTreeMap::from([(
                id.to_owned(),
                Machine {
                    state,
                    machine_id: id.into(),
                    label: id.into(),
                    ..Default::default()
                },
            )])
        };
        let ready = |count| Ready {
            count: Some(count),
            accuracy: "exact",
        };
        store_tick(
            pool,
            MINUTE,
            MINUTE,
            true,
            samples("online", "old"),
            ready(1),
        )
        .await
        .unwrap();
        assert!(
            store_tick(
                pool,
                minute,
                minute,
                true,
                samples("invalid", "broken"),
                ready(10)
            )
            .await
            .is_err()
        );
        let (first, second) = tokio::join!(
            store_tick(
                pool,
                minute,
                minute,
                true,
                samples("online", "first"),
                ready(1)
            ),
            store_tick(
                pool,
                minute,
                minute,
                true,
                samples("online", "second"),
                ready(2)
            ),
        );
        first.unwrap();
        second.unwrap();
        store_tick(
            pool,
            minute,
            minute,
            true,
            samples("online", "replay"),
            ready(100),
        )
        .await
        .unwrap();

        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT machine_id,ready_count FROM performance_machine_minutes
             JOIN performance_fleet_minutes USING (minute_ms)",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0] == ("first".into(), 1) || rows[0] == ("second".into(), 2),
            "{rows:?}",
        );
    }

    #[test]
    fn lowered_slot_limits_preserve_running_occupancy_without_free_capacity() {
        let mut machine = Machine::default();
        machine.record(
            "online",
            slots::SlotView {
                total: 2,
                occupied: 3,
                idle: 0,
                blocked: vec![1, 2, 3],
                all_blocked: false,
            },
        );
        assert_eq!(machine.enabled_occupied_slots, 2);
        machine.cap(1);
        assert_eq!(machine.enabled_slots, 1);
        assert_eq!(machine.enabled_occupied_slots, 1);
        assert_eq!(machine.occupied_slots, 3);
        assert_eq!(machine.free_slots, 0);
    }

    #[test]
    fn expired_heartbeat_and_pause_disable_capacity() {
        let now = DateTime::from_timestamp_millis(120_000).unwrap();
        let seen = DateTime::from_timestamp_millis(59_999).unwrap();
        let ttl = Duration::from_secs(60);
        assert_eq!(status(false, Some(seen), now, ttl), "stale");
        assert_eq!(status(true, Some(now), now, ttl), "paused");
        assert_eq!(status(false, None, now, ttl), "offline");
        assert_eq!(status(false, Some(now), now, ttl), "online");
    }
}

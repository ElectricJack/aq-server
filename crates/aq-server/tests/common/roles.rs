//! The role documents of the selection fixture (spec 0014): the shipped
//! roles, one execution profile that fits the fixture's node, and the nine
//! levels on the fixture's models. One helper installs them and yields the
//! active snapshot.
use aq_profiles::roles::{Kind, SelectionDefaults, Snapshot};
use uuid::Uuid;

/// The fixture node's one harness. `opencode` takes any provider, so the
/// fixture's synthetic providers are valid candidates on it.
pub const HARNESS: &str = "opencode";
pub const ENVIRONMENT: &str = "confined-worktree";
/// The level an unrouted task of the fixture project runs on: the fixture
/// binding names it, so a task routed to `standard-high` is told apart.
pub const DEFAULT_LEVEL: &str = "standard-medium";

/// The execution profile every fixture role runs under. `startup_bytes` is
/// its startup byte limit.
pub fn execution(startup_bytes: u64) -> String {
    format!(
        "---\nid: sandbox\nname: Fixture sandbox\ndescription: The selection fixture's node.\n\
         harnesses: [claude, {HARNESS}]\nenvironment: {ENVIRONMENT}\nconfinement: native\n---\n\
         ## Credentials\n- claude: account-a\n- {HARNESS}: account-a\n\n\
         ## Limits\n- wall_time_ms: 60000\n- memory_bytes: 268435456\n- cpu_millis: 60000\n\
         - process_limit: 8\n- startup_context_bytes: {startup_bytes}\n- max_output_tokens: 128\n"
    )
}
/// A level document with one candidate per `(provider, model, reasoning)`.
pub fn level(id: &str, candidates: &[(&str, &str, &str)]) -> String {
    let rows: String = (candidates.iter())
        .map(|(provider, model, reasoning)| {
            format!("| {provider} | {model} | {reasoning} | {HARNESS} |\n")
        })
        .collect();
    format!(
        "---\nid: {id}\nname: Fixture {id}\ndescription: Fixture level {id}.\n---\n\
         ## Candidates\n| provider | model | reasoning | harnesses |\n|---|---|---|---|\n{rows}"
    )
}
/// [`level`] for models the fixture has no availability observation for
/// (it has one for `model-a` only): an unobserved model may be tried.
pub fn open_level(id: &str, candidates: &[(&str, &str, &str)]) -> String {
    format!(
        "{}\n## Selection\n- allow_unknown_availability: true\n",
        level(id, candidates)
    )
}
/// A candidate row: `(provider, model, reasoning)`.
pub type Candidate = (&'static str, &'static str, &'static str);
/// The fixture's levels: every shipped id. Two models and three efforts, so
/// a routed task shows which level it ran on.
pub fn levels() -> Vec<(&'static str, Vec<Candidate>)> {
    let a = |reasoning| vec![("fixture-a", "model-a", reasoning)];
    let small = |reasoning| vec![("fixture-a", "model-small", reasoning)];
    vec![
        ("simple-low", small("default")),
        ("simple-high", small("high")),
        ("standard-low", a("low")),
        (DEFAULT_LEVEL, a("high")),
        ("standard-high", a("high")),
        ("complex-low", a("low")),
        ("complex-high", a("high")),
        ("deep-low", a("low")),
        ("deep-high", a("high")),
    ]
}
/// Save one document as the owner `actor` would in Settings.
pub async fn save(pool: &sqlx::PgPool, actor: Uuid, kind: Kind, id: &str, markdown: &str) {
    let revision = aq_role_settings::get(pool, kind, id)
        .await
        .unwrap()
        .map_or(0, |d| d.revision);
    aq_role_settings::save(
        aq_role_settings::Writer::new(pool, actor, 1).edit(revision, Uuid::new_v4()),
        kind,
        id,
        markdown,
    )
    .await
    .unwrap_or_else(|error| panic!("save {} `{id}`: {error:?}", kind.label()));
}
/// Whether this store has the role tables. A fixture held at an earlier
/// migration prefix (`with_store_profile`) does not until [`install`].
async fn stored(pool: &sqlx::PgPool) -> bool {
    sqlx::query_scalar("SELECT to_regclass('role_documents') IS NOT NULL")
        .fetch_one(pool)
        .await
        .unwrap()
}
/// Give a store whose `role_settings` predates migration 0142 that
/// migration's delivery columns: the role store reads them with the rest of
/// its settings row. Only the columns, not the format stamp, so the store's
/// format stays at its prefix.
pub async fn add_delivery_settings(pool: &sqlx::PgPool) {
    let migration = include_str!("../../migrations/0142_delivery_settings.sql");
    let (columns, stamp) = migration.split_once("UPDATE meta").unwrap();
    assert!(stamp.contains("state_format_version"));
    sqlx::raw_sql(columns)
        .execute(pool)
        .await
        .expect("delivery settings columns on a historical fixture store");
}
/// Drop the role tables [`install`] gave a historical fixture store, so the
/// store can be upgraded by its registered migrations.
pub async fn remove_from_historical_store(pool: &sqlx::PgPool) {
    sqlx::raw_sql(
        "DROP TABLE task_offer_refusals,attempt_ran_with,role_snapshot_pins,\
         role_settings_operations,role_default_installs,role_settings,role_documents,\
         role_snapshots",
    )
    .execute(pool)
    .await
    .unwrap();
}
/// Pin the active snapshot for `attempt`, as creating the attempt from an
/// offer does (spec 0014 §7.1). The store gets its defaults if it has none.
pub async fn pin_active(pool: &sqlx::PgPool, attempt: Uuid) {
    assert!(stored(pool).await, "this fixture store has no role tables");
    if aq_role_settings::active_snapshot(pool)
        .await
        .unwrap()
        .is_none()
    {
        aq_role_settings::startup(pool, 0).await.unwrap();
    }
    let digest = active(pool).await.digest().to_string();
    let mut conn = pool.acquire().await.unwrap();
    aq_role_settings::pin(&mut conn, &digest, attempt)
        .await
        .unwrap();
    assert!(pinned(pool, attempt).await);
}
/// Whether `attempt` still pins a snapshot once the pins of attempts that
/// no longer hold their task are released, as the next attempt's creation
/// and the next snapshot activation do.
pub async fn pinned(pool: &sqlx::PgPool, attempt: Uuid) -> bool {
    let mut conn = pool.acquire().await.unwrap();
    aq_role_settings::release_finished_pins(&mut conn)
        .await
        .unwrap();
    drop(conn);
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM role_snapshot_pins WHERE attempt_id=$1)")
        .bind(attempt)
        .fetch_one(pool)
        .await
        .unwrap()
}
pub async fn active(pool: &sqlx::PgPool) -> Snapshot {
    aq_role_settings::active_snapshot(pool)
        .await
        .unwrap()
        .expect("an active role snapshot")
        .snapshot
}
/// Install the fixture's documents over the shipped defaults and return the
/// active snapshot: the shipped `developer` and `reviewer` roles, the fixture
/// execution profile, the fixture levels, and the selection defaults the
/// fixture's tests were written for.
pub async fn install(pool: &sqlx::PgPool, actor: Uuid) -> Snapshot {
    let defaults = SelectionDefaults {
        max_attempts: 3,
        observation_max_age_ms: 5_000,
        unknown_cost: aq_profiles::UnknownCostPolicy::Deny,
        ..SelectionDefaults::default()
    };
    let mut documents = vec![(Kind::Execution, "sandbox", execution(8192))];
    for (id, candidates) in levels() {
        let document = if candidates[0].1 == "model-a" {
            level(id, &candidates)
        } else {
            open_level(id, &candidates)
        };
        documents.push((Kind::Intelligence, id, document));
    }
    // A fixture held at a migration prefix from before the role tables
    // (`with_store_profile`) is given them: the role migration creates only
    // its own tables, and the offer path reads the active snapshot from
    // them. The rest of that store, and its format, stay at the prefix.
    if !stored(pool).await {
        let migration = include_str!("../../migrations/0134_roles_and_profiles.sql");
        // Only its first part, which creates the role tables: the rest
        // removes the profile catalog and stamps the store's format.
        let (tables, removal) = migration
            .split_once("-- == The profile catalog is removed ==")
            .unwrap();
        assert!(removal.contains("state_format_version"));
        sqlx::raw_sql(tables)
            .execute(pool)
            .await
            .expect("role tables on a historical fixture store");
        add_delivery_settings(pool).await;
        aq_role_settings::startup(pool, 0)
            .await
            .expect("install the shipped roles and profiles");
    }
    for (kind, id, document) in &documents {
        save(pool, actor, *kind, id, document).await;
    }
    let settings = aq_role_settings::settings(pool).await.unwrap();
    let revision = settings.selection_defaults_revision;
    aq_role_settings::save_selection_defaults(
        aq_role_settings::Writer::new(pool, actor, 1).edit(revision, Uuid::new_v4()),
        &defaults,
    )
    .await
    .unwrap();
    active(pool).await
}
/// A synthetic prepared launch for a test that asks for a selection without
/// the producer: one short startup text on the fixture's harness.
pub fn prepared_launch() -> aq_selection::PreparedLaunch {
    aq_selection::PreparedLaunch {
        digest: aq_admission::Digest::of(b"fixture startup text"),
        harness: HARNESS.into(),
        environment: ENVIRONMENT.into(),
        startup_bytes: 20,
        startup_limit_bytes: 8192,
    }
}

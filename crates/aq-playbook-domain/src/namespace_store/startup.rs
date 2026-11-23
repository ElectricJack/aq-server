//! The durable schema and review prerequisites for opening a playbook owner.
use aq_playbook_host::drive::{Refused, Step};
use sqlx::PgPool;

pub struct BridgeSchema<'a> {
    pub format: u32,
    pub database: Option<&'a str>,
    pub reviews_available: bool,
}
impl BridgeSchema<'_> {
    pub async fn verify(&self, pool: &PgPool) -> Result<(), Refused> {
        self.check_migrations(pool).await?;
        check_tables(pool).await?;
        self.check_namespace(pool).await?;
        self.check_reviews(pool).await
    }

    async fn check_migrations(&self, pool: &PgPool) -> Result<(), Refused> {
        let version: Option<String> =
            sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
                .fetch_optional(pool)
                .await
                .step("reading the schema version")?;
        if self.format < 26 || version.as_deref() != Some(&self.format.to_string()) {
            return Err(Refused("schema version mismatch".into()));
        }
        let migrations: Vec<i32> =
            sqlx::query_scalar("SELECT version FROM schema_migrations ORDER BY version")
                .fetch_all(pool)
                .await
                .step("reading the applied migrations")?;
        if migrations != (1..=self.format as i32).collect::<Vec<_>>() {
            return Err(Refused("applied migrations differ".into()));
        }
        Ok(())
    }

    async fn check_namespace(&self, pool: &PgPool) -> Result<(), Refused> {
        let installed: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version=26)")
                .fetch_one(pool)
                .await
                .step("checking migration 26")?;
        let column: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema='public' AND table_name='playbook_namespaces' AND column_name='catalog_profile')",
        )
        .fetch_one(pool)
        .await
        .step("checking the namespace columns")?;
        let bridge_installed = installed && column;
        if !bridge_installed || self.database != pool.connect_options().get_database() {
            return Err(Refused("playbook schema or database mismatch".into()));
        }
        Ok(())
    }

    async fn check_reviews(&self, pool: &PgPool) -> Result<(), Refused> {
        let budget: i64 =
            sqlx::query_scalar("SELECT count(*) FROM review_owner_budget WHERE singleton")
                .fetch_one(pool)
                .await
                .step("reading the review budget")?;
        if budget != 1 || !self.reviews_available {
            return Err(Refused("review budget or source retention missing".into()));
        }
        Ok(())
    }
}

async fn check_tables(pool: &PgPool) -> Result<(), Refused> {
    for table in [
        "review_owner_budget",
        "review_lineage_budget",
        "review_subjects",
        "human_decision_requests",
        "human_decision_finality",
        "review_owner_outbox",
        "playbook_namespaces",
        "playbook_effects",
        "playbook_outbox",
    ] {
        let present: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(format!("public.{table}"))
            .fetch_one(pool)
            .await
            .step("checking the bridge tables")?;
        if !present {
            return Err(Refused("a bridge table is missing".into()));
        }
    }
    Ok(())
}

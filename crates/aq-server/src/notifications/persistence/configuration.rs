use super::*;
use crate::notifications::config::Endpoint;
use sqlx::postgres::PgRow;

struct Installation<'a> {
    config: &'a Loaded,
}
impl Installation<'_> {
    fn digest(&self) -> Result<String> {
        let shape: Vec<_> = self
            .config
            .endpoints
            .iter()
            .map(|(key, v)| (key, v.revision))
            .collect();
        digest(&(self.config.revision, shape))
    }
    fn verify_head(&self, head: &PgRow, hash: &str) -> Result<()> {
        let previous = head.get::<i64, _>("configuration_revision") as u64;
        let stored: Option<String> = head.get("configuration_digest");
        let matches = self.config.revision > previous
            || (self.config.revision == previous && stored.as_deref() == Some(hash));
        matches.then_some(()).ok_or(Error::Conflict)
    }
    async fn verify_destinations(&self, tx: &mut Transaction<'_, Postgres>) -> Result<()> {
        let retained: Vec<String> =
            sqlx::query_scalar("SELECT endpoint_key FROM notification_destinations")
                .fetch_all(&mut **tx)
                .await?;
        if retained.len()
            + self
                .config
                .endpoints
                .keys()
                .filter(|key| !retained.contains(key))
                .count()
            > 128
        {
            return Err(Error::Overloaded);
        }

        for (key, endpoint) in &self.config.endpoints {
            let previous=sqlx::query("SELECT endpoint_revision,fingerprint FROM notification_destinations WHERE endpoint_key=$1").bind(key).fetch_optional(&mut **tx).await?;
            if previous
                .as_ref()
                .is_some_and(|row| conflicts(endpoint, row))
            {
                return Err(Error::Conflict);
            }
        }
        Ok(())
    }
    async fn publish(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        dispatcher_id: Uuid,
        head: &PgRow,
    ) -> Result<()> {
        let hash = self.digest()?;
        self.verify_head(head, &hash)?;
        self.verify_destinations(tx).await?;
        sqlx::query("UPDATE notification_destinations SET available=FALSE")
            .execute(&mut **tx)
            .await?;
        for (key, endpoint) in &self.config.endpoints {
            sqlx::query("INSERT INTO notification_destinations(endpoint_key,endpoint_revision,configuration_revision,available,fingerprint) VALUES($1,$2,$3,$4,$5) ON CONFLICT(endpoint_key) DO UPDATE SET endpoint_revision=EXCLUDED.endpoint_revision,configuration_revision=EXCLUDED.configuration_revision,available=EXCLUDED.available,fingerprint=CASE WHEN EXCLUDED.endpoint_revision>notification_destinations.endpoint_revision THEN EXCLUDED.fingerprint ELSE COALESCE(EXCLUDED.fingerprint,notification_destinations.fingerprint) END,disabled=CASE WHEN EXCLUDED.endpoint_revision>notification_destinations.endpoint_revision THEN FALSE ELSE notification_destinations.disabled END")
                    .bind(key).bind(revision(endpoint.revision)?).bind(revision(self.config.revision)?).bind(endpoint.webhook.is_some()).bind(&endpoint.fingerprint).execute(&mut **tx).await?;
        }
        sqlx::query("UPDATE notification_budget SET configuration_revision=$1,configuration_digest=$2,dispatcher_id=$3 WHERE singleton").bind(revision(self.config.revision)?).bind(hash).bind(dispatcher_id).execute(&mut **tx).await?;
        Ok(())
    }
}

fn conflicts(endpoint: &Endpoint, previous: &PgRow) -> bool {
    let revision = previous.get::<i64, _>("endpoint_revision") as u64;
    let stored: Option<String> = previous.get("fingerprint");
    let different = stored
        .as_ref()
        .zip(endpoint.fingerprint.as_ref())
        .is_some_and(|(a, b)| a != b);
    endpoint.revision < revision || (endpoint.revision == revision && different)
}

impl Service {
    /// Startup authority publication; a newly installed dispatcher fences the old
    /// process. Missing configuration disables all destinations without deleting
    /// retained message/receipt data. No network or file reads occur here.
    pub async fn install(&self, dispatcher_id: Uuid) -> Result<()> {
        if dispatcher_id.is_nil() {
            return Err(Error::Invalid);
        }
        let mut tx = self.pool.begin().await?;
        lock_budget(&mut tx).await?;
        let head=sqlx::query("SELECT configuration_revision,configuration_digest FROM notification_budget WHERE singleton").fetch_one(&mut *tx).await?;
        if let Some(config) = &self.configuration {
            Installation { config }
                .publish(&mut tx, dispatcher_id, &head)
                .await?;
        } else {
            sqlx::query("UPDATE notification_destinations SET available=FALSE")
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE notification_budget SET dispatcher_id=$1 WHERE singleton")
                .bind(dispatcher_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

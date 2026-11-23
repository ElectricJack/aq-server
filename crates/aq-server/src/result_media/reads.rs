//! List settled media and deliver verified bytes.

use super::{Content, Error, Owner, Result, User, db, files, media, now, sql};
use media::{Receipt, ReportBinding, ReportRevision, Validate, Variant};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

struct ReportTarget {
    id: Uuid,
    revision: ReportRevision,
}

struct ContentTarget {
    report: ReportTarget,
    operation: Uuid,
}

struct ContentSnapshot {
    binding: ReportBinding,
    receipt: Receipt,
}

impl Owner {
    pub async fn list(
        &self,
        actor: &User,
        report: Uuid,
        revision: &str,
    ) -> Result<media::MediaPage> {
        let target = ReportTarget::parse(report, revision)?;
        let mut tx = db::transaction(&self.state).await?;
        db::principal(&mut tx, actor).await?;
        let binding = db::binding(&self.state, &mut tx, target.id, &target.revision).await?;
        let snapshots = self.list_receipts(&mut tx, &target, &binding).await?;
        tx.commit().await.map_err(sql)?;
        let owner = self.clone();
        let items = tokio::task::spawn_blocking(move || {
            snapshots
                .into_iter()
                .map(|(receipt, withheld)| {
                    Ok(media::Artifact {
                        receipt_digest: receipt.digest().map_err(|_| Error::Owner)?,
                        availability: owner.observation(&receipt, withheld),
                        receipt,
                    })
                })
                .collect::<Result<Vec<_>>>()
        })
        .await
        .map_err(|_| Error::Storage)??;
        let mut tx = db::transaction(&self.state).await?;
        db::principal(&mut tx, actor).await?;
        if db::binding(&self.state, &mut tx, target.id, &target.revision).await? != binding {
            return Err(Error::Owner);
        }
        self.check_listed_items(&mut tx, &items).await?;
        let page = media::MediaPage {
            contract: media::Contract::HumanResultMedia1,
            server_time_ms: now(&self.state)?,
            binding,
            limits: self.limits.clone(),
            items,
        };
        page.validate().map_err(|_| Error::Owner)?;
        tx.commit().await.map_err(sql)?;
        Ok(page)
    }
    async fn list_receipts(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        report: &ReportTarget,
        binding: &ReportBinding,
    ) -> Result<Vec<(Receipt, bool)>> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT operation_id FROM result_media_intents WHERE report_id=$1 AND report_revision=$2 AND state='retained' ORDER BY operation_id LIMIT 9",
        )
        .bind(report.id)
        .bind(report.revision.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(sql)?;
        if ids.len() > 8 {
            return Err(Error::Owner);
        }
        let mut snapshots = Vec::new();
        for id in ids {
            let row = db::load(tx, id).await?;
            if row.binding != *binding || row.storage != self.root.id {
                return Err(Error::Owner);
            }
            snapshots.push((row.receipt.ok_or(Error::Owner)?, row.withheld));
        }
        Ok(snapshots)
    }

    async fn check_listed_items(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        items: &[media::Artifact],
    ) -> Result<()> {
        for item in items {
            let current = db::load(tx, item.receipt.artifact_id.as_uuid()).await?;
            if current.receipt.as_ref() != Some(&item.receipt)
                || current.withheld
                    != (item.availability.state == media::AvailabilityState::Withheld)
            {
                return Err(Error::Conflict);
            }
        }
        Ok(())
    }

    pub async fn content(
        &self,
        actor: &User,
        report: Uuid,
        revision: &str,
        id: Uuid,
        preview: bool,
    ) -> Result<Content> {
        let target = ContentTarget {
            report: ReportTarget::parse(report, revision)?,
            operation: id,
        };
        let snapshot = self.load_visible_content(actor, &target).await?;
        let descriptor = if preview {
            snapshot.receipt.variants.preview.clone()
        } else {
            snapshot.receipt.variants.render.clone()
        };
        let (bytes, guard) = self.read_variant(id, preview, &descriptor).await;
        self.check_visible_content(actor, &target, &snapshot)
            .await?;
        let bytes = bytes?;
        Ok(Content {
            bytes,
            _guard: guard.ok_or(Error::Owner)?,
            descriptor,
            receipt_digest: snapshot.receipt.digest().map_err(|_| Error::Owner)?,
        })
    }

    async fn load_visible_content(
        &self,
        actor: &User,
        target: &ContentTarget,
    ) -> Result<ContentSnapshot> {
        let mut tx = db::transaction(&self.state).await?;
        db::principal(&mut tx, actor).await?;
        let binding = db::binding(
            &self.state,
            &mut tx,
            target.report.id,
            &target.report.revision,
        )
        .await?;
        let row = db::load(&mut tx, target.operation).await?;
        if row.binding != binding || row.storage != self.root.id {
            return Err(Error::NotFound);
        }
        if row.withheld {
            return Err(Error::Withheld);
        }
        let receipt = row.receipt.ok_or(Error::NotFound)?;
        if now(&self.state)? >= receipt.retain_until_ms {
            return Err(Error::Expired);
        }
        tx.commit().await.map_err(sql)?;
        Ok(ContentSnapshot { binding, receipt })
    }

    async fn read_variant(
        &self,
        id: Uuid,
        preview: bool,
        descriptor: &Variant,
    ) -> (Result<Vec<u8>>, Option<files::ActivityGuard>) {
        let root = self.root.clone();
        let expected = descriptor.clone();
        match self.root.active(id, false) {
            Ok(guard) => tokio::task::spawn_blocking(move || {
                (
                    root.variant(
                        id,
                        if preview {
                            "preview.png"
                        } else {
                            files::render_name(expected.mime)
                        },
                        &expected,
                    ),
                    Some(guard),
                )
            })
            .await
            .unwrap_or_else(|_| (Err(Error::Storage), None)),
            Err(error) => (Err(error), None),
        }
    }

    async fn check_visible_content(
        &self,
        actor: &User,
        target: &ContentTarget,
        snapshot: &ContentSnapshot,
    ) -> Result<()> {
        let mut tx = db::transaction(&self.state).await?;
        db::principal(&mut tx, actor).await?;
        if db::binding(
            &self.state,
            &mut tx,
            target.report.id,
            &target.report.revision,
        )
        .await?
            != snapshot.binding
        {
            return Err(Error::Owner);
        }
        let current = db::load(&mut tx, target.operation).await?;
        if current.receipt.as_ref() != Some(&snapshot.receipt) || current.cleanup != "none" {
            return Err(Error::NotFound);
        }
        if current.withheld {
            return Err(Error::Withheld);
        }
        if now(&self.state)? >= snapshot.receipt.retain_until_ms {
            return Err(Error::Expired);
        }
        self.root.check()?;
        tx.commit().await.map_err(sql)?;
        Ok(())
    }
}

impl ReportTarget {
    fn parse(id: Uuid, revision: &str) -> Result<Self> {
        Ok(Self {
            id,
            revision: ReportRevision::parse(revision).map_err(|_| Error::Invalid)?,
        })
    }
}

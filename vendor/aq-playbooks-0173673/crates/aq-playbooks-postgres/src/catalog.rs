//! Current metadata is derived inside the same State CAS, never from a local
//! current-catalog cache. Reads require this session's actual live owner lease.
use super::*;
use aq_playbooks_runner::{
    CatalogError, CatalogPage, CatalogQuery, CatalogStamp,
    catalog_inspection::{self, CatalogItem},
};

pub(crate) struct Head {
    pub revision: i64,
    pub metadata_revision: i64,
    pub supported: bool,
    pub definition_count: i64,
    pub activation_count: i64,
}

pub(crate) struct Sync<'a> {
    pub namespace: &'a str,
    pub previous: &'a State,
    pub next: &'a State,
    pub revision: i64,
    pub previous_metadata_revision: Option<i64>,
    pub force_reencrypt: bool,
    pub keyring: Option<&'a ProtectionKeyring>,
}
pub(crate) fn sync(
    tx: &mut ObservedTransaction<'_>,
    input: Sync<'_>,
) -> Result<Head, StorageError> {
    let Sync {
        namespace,
        previous,
        next,
        revision,
        previous_metadata_revision,
        force_reencrypt,
        keyring,
    } = input;
    let materialized = previous_metadata_revision.is_some();
    let previous_rows = if materialized {
        catalog_inspection::project(previous).ok()
    } else {
        None
    };
    let next_rows = catalog_inspection::project(next).ok();
    if !materialized
        && tx
            .query_opt(
                "SELECT 1 FROM aq_playbooks.catalog_rows WHERE namespace=$1 LIMIT 1",
                &[&namespace],
            )
            .map_err(backend)?
            .is_some()
    {
        return Err(backend("unmaterialized catalog unexpectedly contains rows"));
    }
    let metadata_revision = if !materialized || previous_rows != next_rows {
        revision
    } else {
        previous_metadata_revision.ok_or_else(|| backend("catalog revision missing"))?
    };
    let Some(next_rows) = next_rows else {
        tx.execute(
            "DELETE FROM aq_playbooks.catalog_rows WHERE namespace=$1",
            &[&namespace],
        )
        .map_err(backend)?;
        return Ok(Head {
            revision,
            metadata_revision,
            supported: false,
            definition_count: 0,
            activation_count: 0,
        });
    };
    if let Some(previous) = &previous_rows {
        for ((kind, key), _) in previous
            .iter()
            .filter(|(key, _)| !next_rows.contains_key(*key))
        {
            tx.execute("DELETE FROM aq_playbooks.catalog_rows WHERE namespace=$1 AND kind=$2 AND item_key=$3", &[&namespace,&kind.key(),&key]).map_err(backend)?;
        }
    }
    for ((kind, key), item) in &next_rows {
        if !force_reencrypt
            && previous_rows
                .as_ref()
                .and_then(|rows| rows.get(&(*kind, key.clone())))
                == Some(item)
        {
            continue;
        }
        let bytes = Zeroizing::new(serde_json::to_vec(item).map_err(backend)?);
        let (plain, key_id, nonce, encrypted) = if let Some(keys) = keyring {
            let sealed = keys.seal_catalog(
                protection::CatalogContext {
                    namespace,
                    kind: kind.key(),
                    key,
                    revision: revision as u64,
                },
                &bytes,
            )?;
            (
                None,
                Some(sealed.key_id),
                Some(sealed.nonce),
                Some(sealed.ciphertext),
            )
        } else {
            (Some(bytes.to_vec()), None, None, None)
        };
        tx.execute("INSERT INTO aq_playbooks.catalog_rows(namespace,kind,item_key,row_revision,plain_bytes,protection_key_id,protection_nonce,encrypted_bytes) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(namespace,kind,item_key) DO UPDATE SET row_revision=EXCLUDED.row_revision,plain_bytes=EXCLUDED.plain_bytes,protection_key_id=EXCLUDED.protection_key_id,protection_nonce=EXCLUDED.protection_nonce,encrypted_bytes=EXCLUDED.encrypted_bytes", &[&namespace,&kind.key(),&key,&revision,&plain,&key_id,&nonce,&encrypted]).map_err(backend)?;
    }
    Ok(Head {
        revision,
        metadata_revision,
        supported: true,
        definition_count: next_rows
            .keys()
            .filter(|(kind, _)| *kind == catalog_inspection::CatalogKind::Definitions)
            .count()
            .try_into()
            .map_err(backend)?,
        activation_count: next_rows
            .keys()
            .filter(|(kind, _)| *kind == catalog_inspection::CatalogKind::Activations)
            .count()
            .try_into()
            .map_err(backend)?,
    })
}

impl PostgresStorage {
    pub(crate) fn current_catalog(
        &self,
        namespace: &str,
        owner: &str,
        query: &CatalogQuery,
    ) -> Result<CatalogPage, CatalogError> {
        query.validate()?;
        check_name(namespace)?;
        check_name(owner)?;
        let mut conn = self.connection()?;
        let incarnation = conn
            .leases
            .get(namespace)
            .filter(|lease| lease.owner == owner && !lease.catalog_uncertain)
            .ok_or(CatalogError::Ownership)?
            .incarnation;
        let mut tx = ObservedTransaction::new(
            conn.client.transaction().map_err(backend)?,
            self.profiling.clone(),
            "catalog_read",
        );
        let row = tx.query_opt("SELECT revision,owner,incarnation,owner_backend_pid=pg_backend_pid() AS same_backend,writer_format,catalog_projected_revision,catalog_metadata_revision,catalog_supported,protection_key_id,catalog_definition_count,catalog_activation_count,catalog_actual_definition_count,catalog_actual_activation_count FROM aq_playbooks.namespaces WHERE namespace=$1 FOR SHARE", &[&namespace]).map_err(backend)?.ok_or(CatalogError::Ownership)?;
        let advisory_key = lock_key(namespace) as u64;
        let (high, low) = (
            (advisory_key >> 32) as i64,
            (advisory_key & 0xffff_ffff) as i64,
        );
        let lock: bool = tx.query_one("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND pid=pg_backend_pid() AND classid::bigint=$1 AND objid::bigint=$2 AND objsubid=1 AND granted)", &[&high,&low]).map_err(backend)?.get(0);
        if !lock
            || row.get::<_, Option<String>>("owner").as_deref() != Some(owner)
            || row.get::<_, i64>("incarnation") != incarnation
            || row.get::<_, Option<bool>>("same_backend") != Some(true)
        {
            return Err(CatalogError::Ownership);
        }
        if row.get::<_, i32>("writer_format") != 2 {
            return Err(CatalogError::Unsupported);
        }
        let revision: i64 = row.get("revision");
        let projected: Option<i64> = row.get("catalog_projected_revision");
        let Some(projected) = projected else {
            return Err(CatalogError::Unsupported);
        };
        if projected != revision {
            return Err(CatalogError::Backend(
                "catalog projection revision mismatch".into(),
            ));
        }
        validate_counts(&row)?;
        if !row.get::<_, bool>("catalog_supported") {
            return Err(CatalogError::Unsupported);
        }
        let metadata_revision: i64 = row
            .get::<_, Option<i64>>("catalog_metadata_revision")
            .filter(|value| *value > 0 && *value <= revision)
            .ok_or_else(|| CatalogError::Backend("catalog metadata revision invalid".into()))?;
        let stamp = CatalogStamp {
            namespace: namespace.into(),
            kind: query.kind,
            incarnation: incarnation as u64,
            metadata_revision: metadata_revision as u64,
        };
        if query
            .expected
            .as_ref()
            .is_some_and(|expected| expected != &stamp)
        {
            return Err(CatalogError::Conflict);
        }
        tx.phase("catalog_page");
        let limit = (query.limit + 1) as i64;
        let rows = if let Some(key) = &query.exact {
            tx.query("SELECT item_key,row_revision,plain_bytes,protection_key_id,protection_nonce,encrypted_bytes FROM aq_playbooks.catalog_rows WHERE namespace=$1 AND kind=$2 AND item_key=$3", &[&namespace,&query.kind.key(),&key]).map_err(backend)?
        } else if let Some(after) = &query.after {
            tx.query("SELECT item_key,row_revision,plain_bytes,protection_key_id,protection_nonce,encrypted_bytes FROM aq_playbooks.catalog_rows WHERE namespace=$1 AND kind=$2 AND item_key>$3 ORDER BY item_key LIMIT $4", &[&namespace,&query.kind.key(),&after,&limit]).map_err(backend)?
        } else {
            tx.query("SELECT item_key,row_revision,plain_bytes,protection_key_id,protection_nonce,encrypted_bytes FROM aq_playbooks.catalog_rows WHERE namespace=$1 AND kind=$2 ORDER BY item_key LIMIT $3", &[&namespace,&query.kind.key(),&limit]).map_err(backend)?
        };
        let protected = row.get::<_, Option<String>>("protection_key_id").is_some();
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let key: String = row.get("item_key");
            let at: i64 = row.get("row_revision");
            if at <= 0 || at > revision {
                return Err(CatalogError::Backend("catalog row revision invalid".into()));
            }
            let plain: Option<Vec<u8>> = row.get("plain_bytes");
            let key_id: Option<String> = row.get("protection_key_id");
            let nonce: Option<Vec<u8>> = row.get("protection_nonce");
            let encrypted: Option<Vec<u8>> = row.get("encrypted_bytes");
            let bytes = match (protected, plain, key_id, nonce, encrypted) {
                (false, Some(bytes), None, None, None) => Zeroizing::new(bytes),
                (true, None, Some(key_id), Some(nonce), Some(bytes)) => self
                    .keyring
                    .as_ref()
                    .ok_or(CatalogError::Unsupported)?
                    .open_catalog(
                        protection::CatalogContext {
                            namespace,
                            kind: query.kind.key(),
                            key: &key,
                            revision: at as u64,
                        },
                        &key_id,
                        &nonce,
                        &bytes,
                    )?,
                _ => {
                    return Err(CatalogError::Backend(
                        "catalog protection envelope mismatch".into(),
                    ));
                }
            };
            if bytes.len() > catalog_inspection::MAX_CATALOG_ROW_BYTES {
                return Err(CatalogError::Unsupported);
            }
            let item: CatalogItem = serde_json::from_slice(&bytes)
                .map_err(|_| CatalogError::Backend("invalid catalog row encoding".into()))?;
            item.validate()?;
            if item.key != key || item.kind() != query.kind {
                return Err(CatalogError::Backend(
                    "catalog row identity mismatch".into(),
                ));
            }
            items.push(item);
        }
        let page = catalog_inspection::make_page(query, stamp, revision as u64, items, false)?;
        tx.commit().map_err(backend)?;
        Ok(page)
    }
}

pub(crate) fn validate_counts(row: &postgres::Row) -> Result<(), StorageError> {
    for (expected, actual) in [
        (
            "catalog_definition_count",
            "catalog_actual_definition_count",
        ),
        (
            "catalog_activation_count",
            "catalog_actual_activation_count",
        ),
    ] {
        let expected: i64 = row.get(expected);
        let actual: i64 = row.get(actual);
        if expected < 0 || actual < 0 || expected != actual {
            return Err(backend("catalog projection row count mismatch"));
        }
    }
    Ok(())
}

//! Private central reservation validators across frozen migration boundaries.
use super::{MIGRATIONS, StoreError, migrate_known};
use crate::test_support::TestDb;
use sqlx::PgPool;

async fn database(version: u32) -> (TestDb, PgPool) {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_known(&pool, &MIGRATIONS[..version as usize], version)
        .await
        .unwrap();
    (db, pool)
}
#[tokio::test]
async fn schema_shapes_follow_frozen_migration_boundaries() {
    let (_db, pool) = database(37).await;
    for version in [48, 49, 108, 109, crate::STATE_FORMAT_VERSION] {
        migrate_known(&pool, &MIGRATIONS[..version as usize], version)
            .await
            .unwrap();
        for name in [
            "central_supervisor_reservations_state_check",
            "central_supervisor_reservations_check2",
        ] {
            let mut tx = pool.begin().await.unwrap();
            sqlx::raw_sql(&format!(
                "ALTER TABLE central_supervisor_reservations DROP CONSTRAINT {name}; \
                 ALTER TABLE central_supervisor_reservations ADD CONSTRAINT {name} CHECK(TRUE)"
            ))
            .execute(&mut *tx)
            .await
            .unwrap();
            assert!(
                matches!(
                    super::central37::central37_storage_guard(&mut tx, version).await,
                    Err(StoreError::Corrupt(_))
                ),
                "{name} at {version}"
            );
            tx.rollback().await.unwrap();
        }
    }
}

#[test]
fn function_bodies_follow_frozen_migration_boundaries() {
    for name in [
        "central_global_mailbox_key",
        "central_global_mail_source_valid",
        "guard_central_global_mail",
        "guard_central_reservation_actor",
        "guard_central_reservation_state",
        "guard_central_reservation_immutable",
        "check_central_reservation_consistency",
        "central_supervisor_turn_complete",
        "consume_central_supervisor_turn",
        "unknown_function",
    ] {
        let mut expected = None;
        for (version, sql) in MIGRATIONS.iter().filter(|(version, _)| *version >= 37) {
            if let Some(body) = aq_profiled_state::sql_function_body(sql, name) {
                expected = Some(body);
            }
            let actual = super::central37::function_body(name, *version as u32);
            match expected {
                Some(body) => assert_eq!(actual.unwrap(), body, "{name} at {version}"),
                None => assert!(
                    matches!(actual, Err(StoreError::Corrupt(_))),
                    "{name} at {version}"
                ),
            }
        }
    }
}

//! Actual SQL retention; installation/private-binding claims remain synthetic.
use super::*;
use sqlx::Row;
async fn row(c: &Prepared) -> sqlx::postgres::PgRow {
    sqlx::query("SELECT operation_id,receipt_digest,receipt FROM admission_material_receipts WHERE lease_id=$1").bind(c.plan.materials[0].lease_id).fetch_one(&c.case.env.env.pool).await.unwrap()
}
#[tokio::test]
async fn installation_receipt_is_atomic_exact_and_replay_never_reconstructs_a_missing_row() {
    let c = setup().await;
    let first = grant(&c).await.unwrap();
    let r = row(&c).await;
    assert_eq!(
        r.get::<Uuid, _>("operation_id"),
        c.request.materials[0].operation_id
    );
    assert_eq!(
        r.get::<Value, _>("receipt"),
        serde_json::to_value(&c.request.materials[0]).unwrap()
    );
    assert_eq!(
        r.get::<String, _>("receipt_digest"),
        aq_selection::digest(&c.request.materials[0])
            .unwrap()
            .as_str()
    );
    let again = grant(&c).await.unwrap();
    assert_eq!(first.original.grant, again.original.grant);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admission_material_receipts")
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    sqlx::query("DELETE FROM admission_material_receipts WHERE lease_id=$1")
        .bind(c.plan.materials[0].lease_id)
        .execute(&c.case.env.env.pool)
        .await
        .unwrap();
    assert!(grant(&c).await.is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admission_material_receipts")
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
#[tokio::test]
async fn existing_or_replayed_changed_installation_claims_are_refused() {
    for corruption in 0..3 {
        let c = setup().await;
        grant(&c).await.unwrap();
        match corruption {
            0 => {
                sqlx::query("UPDATE admission_material_receipts SET operation_id=$1")
                    .bind(Uuid::new_v4())
                    .execute(&c.case.env.env.pool)
                    .await
                    .unwrap();
            }
            1 => {
                sqlx::query("UPDATE admission_material_receipts SET receipt_digest=$1")
                    .bind("a".repeat(64))
                    .execute(&c.case.env.env.pool)
                    .await
                    .unwrap();
            }
            _ => {
                let mut receipt = c.request.materials[0].clone();
                receipt.private_binding.digest =
                    Digest::of(b"different synthetic private installation");
                sqlx::query("UPDATE admission_material_receipts SET receipt=$1,receipt_digest=$2")
                    .bind(serde_json::to_value(&receipt).unwrap())
                    .bind(aq_selection::digest(&receipt).unwrap().as_str())
                    .execute(&c.case.env.env.pool)
                    .await
                    .unwrap();
            }
        }
        assert!(grant(&c).await.is_err());
    }
    let c = setup().await;
    let mut receipt = c.request.materials[0].clone();
    receipt.private_binding.digest = Digest::of(b"preexisting different installer claim");
    sqlx::query("INSERT INTO admission_material_receipts(lease_id,operation_id,receipt_digest,receipt) VALUES($1,$2,$3,$4)").bind(receipt.lease_id).bind(receipt.operation_id).bind(aq_selection::digest(&receipt).unwrap().as_str()).bind(serde_json::to_value(&receipt).unwrap()).execute(&c.case.env.env.pool).await.unwrap();
    assert!(grant(&c).await.is_err());
    ungranted(&c).await;
}
#[tokio::test]
async fn grant_failure_rolls_back_new_installation_receipt_with_authority() {
    let c = setup().await;
    sqlx::raw_sql("CREATE FUNCTION refuse_prepare_receipt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='prepare' THEN RAISE EXCEPTION 'synthetic late commit failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER refuse_prepare_receipt BEFORE INSERT ON profiled_execution_receipts FOR EACH ROW EXECUTE FUNCTION refuse_prepare_receipt();").execute(&c.case.env.env.pool).await.unwrap();
    assert!(grant(&c).await.is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admission_material_receipts")
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    ungranted(&c).await;
}
#[tokio::test]
async fn retained_installation_json_bytes_refuse_new_grant_under_shared_budget() {
    let c = setup().await;
    let pool = &c.case.env.env.pool;
    // Synthetic historical storage padding, deliberately not a usable receipt.
    // Quotas must count all live retained bytes, including unusable/corrupt history.
    // Spec 0008: the padding leases are pending, hence live; settled ones no longer count.
    sqlx::query("INSERT INTO admission_material_leases(lease_id,operation_id,request_digest,node_id,boot_id,prospective_attempt_id,admitted_attempt_id,offer_id,credential_identity,credential_revision,purpose,lease,verifier,state,issued_at_ms,expires_at_ms) SELECT gen_random_uuid(),gen_random_uuid(),m.request_digest,m.node_id,m.boot_id,gen_random_uuid(),NULL,gen_random_uuid(),'synthetic-quota',1,'synthetic-quota','{}'::jsonb,md5(g::text)||md5((g+1000)::text),'pending',1,2 FROM admission_material_leases m CROSS JOIN generate_series(1,66) g WHERE m.lease_id=$1").bind(c.plan.materials[0].lease_id).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO admission_material_receipts(lease_id,operation_id,receipt_digest,receipt) SELECT lease_id,operation_id,repeat('a',64),jsonb_build_object('synthetic_retained_padding',repeat('x',64000)) FROM admission_material_leases WHERE purpose='synthetic-quota'").execute(pool).await.unwrap();
    let bytes:i64=sqlx::query_scalar("SELECT sum(octet_length(r.receipt::text)) FROM admission_material_receipts r JOIN admission_material_leases m USING(lease_id) WHERE m.node_id=$1").bind(c.request.identity.node_id).fetch_one(pool).await.unwrap();
    assert!(bytes > 4194304);
    assert!(grant(&c).await.is_err());
    ungranted(&c).await;
    let own: i64 =
        sqlx::query_scalar("SELECT count(*) FROM admission_material_receipts WHERE lease_id=$1")
            .bind(c.plan.materials[0].lease_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(own, 0);
}

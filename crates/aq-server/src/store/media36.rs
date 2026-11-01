use super::{Result, StoreError};
use sqlx::{Postgres, Transaction};

pub(super) async fn media36_storage_guard(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    let complete: bool = sqlx::query_scalar(r#"SELECT
      to_regclass('result_media_storage') IS NOT NULL
      AND to_regclass('result_media_budgets') IS NOT NULL
      AND to_regclass('result_media_intents') IS NOT NULL
      AND to_regprocedure('guard_result_media_storage()') IS NOT NULL
      AND to_regprocedure('guard_result_media_budget()') IS NOT NULL
      AND to_regprocedure('guard_result_media_intent()') IS NOT NULL
      AND NOT EXISTS(SELECT 1 FROM (VALUES
        ('result_media_storage','result_media_storage_update_guard',19,'guard_result_media_storage()'),
        ('result_media_storage','result_media_storage_delete_guard',11,'guard_result_media_storage()'),
        ('result_media_storage','result_media_storage_truncate_guard',34,'guard_result_media_storage()'),
        ('result_media_budgets','result_media_budget_insert_guard',7,'guard_result_media_budget()'),
        ('result_media_budgets','result_media_budget_update_guard',19,'guard_result_media_budget()'),
        ('result_media_budgets','result_media_budget_delete_guard',11,'guard_result_media_budget()'),
        ('result_media_budgets','result_media_budget_truncate_guard',34,'guard_result_media_budget()'),
        ('result_media_intents','result_media_intent_insert_guard',7,'guard_result_media_intent()'),
        ('result_media_intents','result_media_intent_update_guard',19,'guard_result_media_intent()'),
        ('result_media_intents','result_media_intent_delete_guard',11,'guard_result_media_intent()'),
        ('result_media_intents','result_media_intent_truncate_guard',34,'guard_result_media_intent()')
      ) expected(rel,name,kind,func) WHERE NOT EXISTS(SELECT 1 FROM pg_trigger t WHERE t.tgrelid=to_regclass(expected.rel) AND t.tgname=expected.name AND t.tgtype=expected.kind AND NOT t.tgisinternal AND t.tgenabled IN ('O','A') AND t.tgfoid=to_regprocedure(expected.func)))
      AND NOT EXISTS(SELECT 1 FROM (VALUES
        ('result_media_storage','storage_id','uuid'),('result_media_storage','root_fingerprint','text'),
        ('result_media_budgets','reserved_bytes','int8'),('result_media_budgets','retained_bytes','int8'),('result_media_budgets','historical_intents','int8'),
        ('result_media_intents','actor_identity','jsonb'),('result_media_intents','report_binding','jsonb'),('result_media_intents','request_bytes','bytea'),('result_media_intents','receipt_bytes','bytea'),
        ('result_media_intents','lease_id','uuid'),('result_media_intents','file_generation','uuid'),('result_media_intents','cleanup_generation','uuid'),('result_media_intents','cleanup_completed_at_ms','int8')
      ) expected(rel,name,kind) WHERE NOT EXISTS(SELECT 1 FROM information_schema.columns c WHERE c.table_schema='public' AND c.table_name=expected.rel AND c.column_name=expected.name AND c.udt_name=expected.kind))
      AND NOT EXISTS(SELECT 1 FROM (VALUES
        ('result_media_budgets','result_media_budget_capacity_check'),
        ('result_media_intents','result_media_lease_pair_check'),('result_media_intents','result_media_receipt_pair_check'),('result_media_intents','result_media_retained_state_check')
      ) expected(rel,name) WHERE NOT EXISTS(SELECT 1 FROM pg_constraint c WHERE c.conrelid=to_regclass(expected.rel) AND c.conname=expected.name AND c.contype='c' AND c.convalidated))
      AND NOT EXISTS(SELECT 1 FROM (VALUES
        ('result_media_exact_report'),('result_media_actor_lookup'),('result_media_project_budget'),('result_media_pending_cleanup'),('result_media_retained_cleanup')
      ) expected(name) WHERE NOT EXISTS(SELECT 1 FROM pg_index i WHERE i.indexrelid=to_regclass(expected.name) AND i.indrelid=to_regclass('result_media_intents') AND i.indisvalid AND i.indisready))
      AND NOT EXISTS(SELECT 1 FROM (VALUES
        ('result_media_intents_actor_id_fkey','users'),('result_media_intents_report_id_fkey','profiled_result_reports'),('result_media_intents_storage_id_fkey','result_media_storage')
      ) expected(name,target) WHERE NOT EXISTS(SELECT 1 FROM pg_constraint c WHERE c.conrelid=to_regclass('result_media_intents') AND c.conname=expected.name AND c.contype='f' AND c.convalidated AND c.confrelid=to_regclass(expected.target)))
    "#).fetch_one(&mut **tx).await?;
    if !complete {
        return Err(StoreError::Corrupt(
            "result media owner storage/guards missing or disabled".into(),
        ));
    }
    let accounted: bool = sqlx::query_scalar(r#"WITH charges AS (
      SELECT 'installation'::TEXT AS scope_key,COALESCE(sum(reserved_bytes),0) AS reserved,COALESCE(sum(retained_bytes),0) AS retained,count(*) AS history FROM result_media_intents
      UNION ALL SELECT 'project/'||project_id::TEXT,COALESCE(sum(reserved_bytes),0),COALESCE(sum(retained_bytes),0),count(*) FROM result_media_intents GROUP BY project_id
    ) SELECT EXISTS(SELECT 1 FROM result_media_budgets WHERE scope_key='installation')
      AND NOT EXISTS(SELECT 1 FROM charges c FULL OUTER JOIN result_media_budgets b USING(scope_key)
       WHERE b.scope_key IS NULL OR b.reserved_bytes<>COALESCE(c.reserved,0) OR b.retained_bytes<>COALESCE(c.retained,0) OR b.historical_intents<>COALESCE(c.history,0))
      AND (SELECT count(*)<=1 AND COALESCE(bool_and(singleton),TRUE) FROM result_media_storage)
    "#).fetch_one(&mut **tx).await?;
    if !accounted {
        return Err(StoreError::Corrupt(
            "result media byte/history accounting is inconsistent".into(),
        ));
    }
    Ok(())
}

use super::{Result, StoreError};
use sqlx::{Postgres, Transaction};

pub(super) async fn media36_storage_guard(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    // A surviving function OID/name is insufficient after CREATE OR REPLACE.
    // Compare the installed body with the exact frozen migration body too.
    let migration = include_str!("../../migrations/0036_result_media_owner.sql");
    for name in [
        "guard_result_media_storage",
        "guard_result_media_budget",
        "guard_result_media_intent",
    ] {
        let expected = migration
            .split_once(&format!("CREATE FUNCTION {name}()"))
            .and_then(|(_, tail)| tail.split_once("AS $$"))
            .and_then(|(_, tail)| tail.split_once("$$;"))
            .map(|(body, _)| body)
            .ok_or_else(|| StoreError::Corrupt("result media guard source missing".into()))?;
        let actual: Option<String> =
            sqlx::query_scalar("SELECT prosrc FROM pg_proc WHERE oid=to_regprocedure($1)")
                .bind(format!("{name}()"))
                .fetch_optional(&mut **tx)
                .await?;
        if actual.as_deref() != Some(expected) {
            return Err(StoreError::Corrupt(
                "result media guard body drifted".into(),
            ));
        }
    }
    let complete: bool = sqlx::query_scalar(r#"SELECT
      to_regclass('result_media_storage') IS NOT NULL
      AND to_regclass('result_media_budgets') IS NOT NULL
      AND to_regclass('result_media_intents') IS NOT NULL
      AND to_regprocedure('guard_result_media_storage()') IS NOT NULL
      AND to_regprocedure('guard_result_media_budget()') IS NOT NULL
      AND to_regprocedure('guard_result_media_intent()') IS NOT NULL
      AND NOT EXISTS(SELECT 1 FROM (VALUES
        ('result_media_storage','result_media_storage_pkey','p','singleton'),('result_media_storage','result_media_storage_storage_id_key','u','storage_id'),
        ('result_media_budgets','result_media_budgets_pkey','p','scope_key'),('result_media_intents','result_media_intents_pkey','p','operation_id')
      ) expected(rel,name,kind,column_name) WHERE NOT EXISTS(SELECT 1 FROM pg_constraint c WHERE c.conrelid=to_regclass(expected.rel) AND c.conname=expected.name AND c.contype::TEXT=expected.kind AND c.convalidated AND NOT c.condeferrable AND c.conkey=ARRAY[(SELECT attnum FROM pg_attribute WHERE attrelid=c.conrelid AND attname=expected.column_name)]))
      AND NOT EXISTS(SELECT 1 FROM (VALUES
        ('result_media_storage',ARRAY['singleton','storage_id','root_fingerprint','created_at_ms']),
        ('result_media_budgets',ARRAY['scope_key','limit_bytes','reserved_bytes','retained_bytes','historical_intents']),
        ('result_media_intents',ARRAY['operation_id','actor_id','actor_identity','report_id','report_revision','project_id','report_binding','storage_id','rendering_policy','admission_limits','request_bytes','request_digest','input_digest','input_bytes','filename','capture','state','created_at_ms','upload_expires_at_ms','retain_until_ms','transition_at_ms','reserved_bytes','retained_bytes','processing_attempts','file_generation','cleanup_state'])
      ) expected(rel,columns),unnest(expected.columns) name WHERE NOT EXISTS(SELECT 1 FROM pg_attribute a WHERE a.attrelid=to_regclass(expected.rel) AND a.attname=name AND a.attnotnull AND NOT a.attisdropped))
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
        ('result_media_budgets','result_media_budget_capacity_check','((reserved_bytes + retained_bytes) <= limit_bytes)'),
        ('result_media_intents','result_media_lease_pair_check','((lease_id IS NULL) = (lease_expires_at_ms IS NULL))'),
        ('result_media_intents','result_media_receipt_pair_check','((receipt_bytes IS NULL) = (receipt_digest IS NULL))'),
        ('result_media_intents','result_media_retained_state_check','((state = ''retained''::text) = (receipt_bytes IS NOT NULL))')
      ) expected(rel,name,expression) WHERE NOT EXISTS(SELECT 1 FROM pg_constraint c WHERE c.conrelid=to_regclass(expected.rel) AND c.conname=expected.name AND c.contype='c' AND c.convalidated AND pg_get_expr(c.conbin,c.conrelid)=expected.expression))
      AND NOT EXISTS(SELECT 1 FROM (VALUES
        ('result_media_exact_report',ARRAY['report_id','report_revision','operation_id'],'(state = ''retained''::text)'),
        ('result_media_actor_lookup',ARRAY['actor_id','operation_id'],NULL),
        ('result_media_project_budget',ARRAY['project_id'],NULL),
        ('result_media_pending_cleanup',ARRAY['upload_expires_at_ms','operation_id'],'((state = ANY (ARRAY[''reserved''::text, ''received''::text, ''finalizing''::text, ''rejected''::text, ''expired''::text])) AND (cleanup_state <> ''done''::text))'),
        ('result_media_retained_cleanup',ARRAY['retain_until_ms','operation_id'],'((state = ''retained''::text) AND (cleanup_state <> ''done''::text))')
      ) expected(name,columns,predicate) WHERE NOT EXISTS(SELECT 1 FROM pg_index i JOIN pg_class ic ON ic.oid=i.indexrelid JOIN pg_am am ON am.oid=ic.relam WHERE i.indexrelid=to_regclass(expected.name) AND i.indrelid=to_regclass('result_media_intents') AND i.indisvalid AND i.indisready AND NOT i.indisunique AND i.indexprs IS NULL AND i.indnatts=i.indnkeyatts AND am.amname='btree' AND pg_get_expr(i.indpred,i.indrelid) IS NOT DISTINCT FROM expected.predicate AND ARRAY(SELECT a.attname::TEXT FROM unnest(i.indkey) WITH ORDINALITY k(num,ord) JOIN pg_attribute a ON a.attrelid=i.indrelid AND a.attnum=k.num ORDER BY k.ord)=expected.columns))
      AND NOT EXISTS(SELECT 1 FROM (VALUES
        ('result_media_intents_actor_id_fkey','users','actor_id','id'),('result_media_intents_report_id_fkey','profiled_result_reports','report_id','report_id'),('result_media_intents_storage_id_fkey','result_media_storage','storage_id','storage_id')
      ) expected(name,target,local_column,foreign_column) WHERE NOT EXISTS(SELECT 1 FROM pg_constraint c WHERE c.conrelid=to_regclass('result_media_intents') AND c.conname=expected.name AND c.contype='f' AND c.convalidated AND c.confrelid=to_regclass(expected.target) AND NOT c.condeferrable AND c.confupdtype='a' AND c.confdeltype='a' AND c.confmatchtype='s' AND c.conkey=ARRAY[(SELECT attnum FROM pg_attribute WHERE attrelid=c.conrelid AND attname=expected.local_column)] AND c.confkey=ARRAY[(SELECT attnum FROM pg_attribute WHERE attrelid=c.confrelid AND attname=expected.foreign_column)]))
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

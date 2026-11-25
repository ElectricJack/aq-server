//! Startup attestation of storage predating the dedicated versioned guards.
use super::{Result, StoreError};
use sqlx::{Postgres, Transaction};

pub(super) async fn validate(tx: &mut Transaction<'_, Postgres>, supported: u32) -> Result<()> {
    if supported >= 50 {
        validate_delegated_permits(tx).await?;
    }
    if supported >= 42 {
        validate_dashboard_operations(tx).await?;
    }
    if supported >= 30 {
        validate_accepted_preparation(tx).await?;
    }
    if supported >= 31 {
        validate_scoped_presence(tx).await?;
    }
    if supported >= 32 {
        validate_phase_execution(tx).await?;
    }
    if supported >= 33 {
        validate_phase_terminal(tx, supported).await?;
    }
    if supported >= 34 {
        validate_human_graph(tx).await?;
    }
    if supported >= 35 {
        validate_supervisor_conversation(tx).await?;
    }
    Ok(())
}

async fn validate_delegated_permits(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    let intact: bool = sqlx::query_scalar(
        r#"SELECT
          to_regclass('delegated_permits') IS NOT NULL
          AND to_regclass('delegated_permit_current') IS NOT NULL
          AND to_regclass('delegated_node_boots') IS NOT NULL
          AND to_regclass('delegated_operations') IS NOT NULL
          AND to_regclass('delegated_permits_verifier') IS NOT NULL
          AND to_regclass('delegated_current_verifier') IS NOT NULL
          AND to_regclass('delegated_current_attempt') IS NOT NULL
          AND to_regclass('delegated_current_channel') IS NOT NULL AND NOT EXISTS(
            SELECT 1 FROM (VALUES
              ('delegated_permits','delegated_permit_guard','delegated_permit_guard()',31),
              ('delegated_permits','delegated_permit_truncate_guard','delegated_permit_guard()',34),
              ('nodes','delegated_node_revoke','delegated_node_revoke()',17),
              ('profiled_cancellations','delegated_attempt_revoke','delegated_attempt_revoke()',5)
            ) expected(rel,name,func,kind) WHERE NOT EXISTS(
              SELECT 1 FROM pg_trigger t WHERE t.tgrelid=to_regclass(expected.rel)
                AND t.tgname=expected.name AND t.tgtype=expected.kind
                AND NOT t.tgisinternal AND t.tgenabled IN ('O','A')
                AND t.tgfoid=to_regprocedure(expected.func)))"#,
    )
    .fetch_one(&mut **tx)
    .await?;
    if !intact {
        return Err(StoreError::Corrupt(
            "delegated permit persistence safeguards missing or disabled".into(),
        ));
    }
    Ok(())
}

async fn validate_dashboard_operations(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    let intact: bool = sqlx::query_scalar("SELECT to_regclass('dashboard_operations') IS NOT NULL AND to_regprocedure('guard_dashboard_operation()') IS NOT NULL AND EXISTS (SELECT 1 FROM pg_trigger WHERE tgname='dashboard_operation_guard' AND tgrelid=to_regclass('dashboard_operations') AND NOT tgisinternal AND tgenabled='O') AND EXISTS (SELECT 1 FROM pg_trigger WHERE tgname='dashboard_operation_truncate_guard' AND tgrelid=to_regclass('dashboard_operations') AND NOT tgisinternal AND tgenabled='O')").fetch_one(&mut **tx).await?;
    if !intact {
        return Err(StoreError::Corrupt(
            "dashboard operation journal safeguards missing".into(),
        ));
    }
    Ok(())
}

async fn validate_accepted_preparation(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    let complete:bool=sqlx::query_scalar("SELECT to_regclass('profiled_v2_proposals') IS NOT NULL AND to_regclass('profiled_v2_accepts') IS NOT NULL").fetch_one(&mut **tx).await?;
    if !complete {
        return Err(StoreError::Corrupt(
            "accepted preparation v2 storage missing from current schema".into(),
        ));
    }
    Ok(())
}

async fn validate_scoped_presence(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    let complete: bool = sqlx::query_scalar("SELECT to_regprocedure('profiled_provisionable_binding_set(jsonb)') IS NOT NULL AND EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema='public' AND table_name='profiled_inventory_semantic_heads' AND column_name='provisionable_binding_set' AND data_type='jsonb')")
        .fetch_one(&mut **tx).await?;
    if !complete {
        return Err(StoreError::Corrupt(
            "scoped presence binding barrier missing from current schema".into(),
        ));
    }
    Ok(())
}

async fn validate_phase_execution(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    let complete: bool = sqlx::query_scalar("SELECT to_regclass('profiled_phase_execution_grants') IS NOT NULL AND to_regprocedure('profiled_phase_execution_guard()') IS NOT NULL AND EXISTS(SELECT 1 FROM pg_trigger WHERE tgname='profiled_phase_execution_immutable' AND tgrelid=to_regclass('profiled_phase_execution_grants') AND NOT tgisinternal AND tgenabled='O')")
        .fetch_one(&mut **tx).await?;
    if !complete {
        return Err(StoreError::Corrupt(
            "phase execution history storage missing from current schema".into(),
        ));
    }
    Ok(())
}

async fn validate_phase_terminal(tx: &mut Transaction<'_, Postgres>, supported: u32) -> Result<()> {
    let complete:bool=sqlx::query_scalar(r#"SELECT
          to_regclass('profiled_phase_terminal_capacity') IS NOT NULL
          AND to_regprocedure('profiled_phase_terminal_capacity_guard()') IS NOT NULL
          AND to_regprocedure('profiled_phase_terminal_reserve()') IS NOT NULL
          AND to_regprocedure('profiled_phase_terminal_origin_guard()') IS NOT NULL
          AND to_regprocedure('profiled_phase_terminal_extra_bytes(text,text,text,bytea,bytea,jsonb)') IS NOT NULL
          AND to_regprocedure('profiled_phase_terminal_used_bytes(uuid)') IS NOT NULL
          AND to_regprocedure('profiled_phase_unqualified_result_guard()') IS NOT NULL
          AND NOT EXISTS(
            SELECT 1 FROM (VALUES
             ('profiled_phase_terminal_capacity','profiled_phase_terminal_capacity_guard'),
             ('profiled_phase_execution_grants','profiled_phase_terminal_reserve'),
             ('profiled_started_receipts','profiled_phase_terminal_origin_guard'),
             ('profiled_stopped_receipts','profiled_phase_terminal_origin_guard'),
             ('profiled_launch_closures','profiled_phase_terminal_origin_guard'),
             ('profiled_result_reports','profiled_phase_unqualified_result_guard'),
             ('profiled_verified_releases',CASE WHEN $1>=39 THEN 'profiled_phase_qualified_release_guard' ELSE 'profiled_phase_unqualified_release_guard' END)
            ) expected(rel,name) WHERE NOT EXISTS(SELECT 1 FROM pg_trigger t WHERE t.tgrelid=to_regclass(expected.rel) AND t.tgname=expected.name AND NOT t.tgisinternal AND t.tgenabled IN ('O','A') AND t.tgfoid=to_regprocedure(CASE WHEN expected.name='profiled_phase_unqualified_release_guard' THEN 'profiled_phase_unqualified_result_guard()' WHEN expected.name='profiled_phase_qualified_release_guard' THEN 'settlement_release_guard()' ELSE expected.name||'()' END)))
          AND NOT EXISTS(
            SELECT 1 FROM (VALUES ('profiled_started_receipts'),('profiled_stopped_receipts'),('profiled_launch_closures')) expected(rel)
            CROSS JOIN (VALUES ('initial_authority_kind','text'),('initial_authority_receipt_digest','text'),('phase_request_digest','text'),('phase_envelope_bytes','bytea'),('phase_ack_bytes','bytea'),('phase_terminal_qualification','jsonb')) col(name,kind)
            WHERE NOT EXISTS(SELECT 1 FROM information_schema.columns c WHERE c.table_schema='public' AND c.table_name=expected.rel AND c.column_name=col.name AND c.udt_name=col.kind))
          AND NOT EXISTS(
            SELECT 1 FROM (VALUES ('profiled_started_receipts'),('profiled_stopped_receipts'),('profiled_launch_closures')) expected(rel)
            CROSS JOIN (VALUES ('phase_envelope_digest'),('phase_ack_digest')) col(name)
            WHERE NOT EXISTS(SELECT 1 FROM pg_attribute a WHERE a.attrelid=to_regclass(expected.rel) AND a.attname=col.name AND a.attgenerated='s' AND NOT a.attisdropped))
          AND NOT EXISTS(SELECT 1 FROM (VALUES ('profiled_started_receipts'),('profiled_stopped_receipts'),('profiled_launch_closures')) expected(rel) WHERE NOT EXISTS(SELECT 1 FROM pg_constraint c WHERE c.conrelid=to_regclass(expected.rel) AND c.conname=expected.rel||'_phase_origin_check' AND c.contype='c' AND c.convalidated))
          AND NOT EXISTS(
            SELECT 1 FROM (VALUES ('legacy_origin_operation_id'),('phase_origin_operation_id')) col(name)
            WHERE NOT EXISTS(SELECT 1 FROM pg_attribute a WHERE a.attrelid=to_regclass('profiled_launch_closures') AND a.attname=col.name AND a.attgenerated='s' AND a.atttypid='uuid'::regtype AND NOT a.attisdropped))
          AND EXISTS(SELECT 1 FROM pg_constraint WHERE conrelid=to_regclass('profiled_launch_closures') AND conname='profiled_launch_closures_legacy_origin_fk' AND contype='f' AND convalidated AND confrelid=to_regclass('profiled_execution_receipts'))
          AND EXISTS(SELECT 1 FROM pg_constraint WHERE conrelid=to_regclass('profiled_launch_closures') AND conname='profiled_launch_closures_phase_origin_fk' AND contype='f' AND convalidated AND confrelid=to_regclass('profiled_phase_execution_grants'))
        "#).bind(supported as i32).fetch_one(&mut **tx).await?;
    if !complete {
        return Err(StoreError::Corrupt(
            "phase terminal origin/capacity storage missing or disabled in current schema".into(),
        ));
    }
    Ok(())
}

async fn validate_human_graph(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    let complete: bool = sqlx::query_scalar("SELECT to_regclass('human_graph_operations') IS NOT NULL AND to_regclass('human_graph_operation_actor') IS NOT NULL AND to_regprocedure('guard_human_graph_operation_identity()') IS NOT NULL AND NOT EXISTS(SELECT 1 FROM (VALUES ('human_graph_operation_identity_guard',19),('human_graph_operation_delete_guard',11),('human_graph_operation_truncate_guard',34)) expected(name,kind) WHERE NOT EXISTS(SELECT 1 FROM pg_trigger t WHERE t.tgrelid=to_regclass('human_graph_operations') AND t.tgname=expected.name AND t.tgtype=expected.kind AND NOT t.tgisinternal AND t.tgenabled IN ('O','A') AND t.tgfoid=to_regprocedure('guard_human_graph_operation_identity()')))")
        .fetch_one(&mut **tx).await?;
    if !complete {
        return Err(StoreError::Corrupt(
            "human Graph intent journal storage missing from current schema".into(),
        ));
    }
    Ok(())
}

async fn validate_supervisor_conversation(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    let complete: bool = sqlx::query_scalar("SELECT to_regclass('global_supervisor_conversation') IS NOT NULL AND to_regprocedure('guard_global_supervisor_conversation()') IS NOT NULL AND NOT EXISTS(SELECT 1 FROM (VALUES ('global_supervisor_conversation_identity_guard',19),('global_supervisor_conversation_delete_guard',11),('global_supervisor_conversation_truncate_guard',34)) expected(name,kind) WHERE NOT EXISTS(SELECT 1 FROM pg_trigger t WHERE t.tgrelid=to_regclass('global_supervisor_conversation') AND t.tgname=expected.name AND t.tgtype=expected.kind AND NOT t.tgisinternal AND t.tgenabled IN ('O','A') AND t.tgfoid=to_regprocedure('guard_global_supervisor_conversation()')))")
        .fetch_one(&mut **tx).await?;
    if !complete {
        return Err(StoreError::Corrupt(
            "global supervisor conversation storage missing or disabled".into(),
        ));
    }
    let singleton: bool = sqlx::query_scalar("SELECT count(*)=1 AND COALESCE(bool_and(singleton AND revision=1 AND conversation_id<>'00000000-0000-0000-0000-000000000000'::uuid),FALSE) FROM global_supervisor_conversation")
        .fetch_one(&mut **tx).await?;
    if !singleton {
        return Err(StoreError::Corrupt(
            "global supervisor conversation singleton missing or invalid".into(),
        ));
    }
    Ok(())
}

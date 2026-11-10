//! Exact catalog shapes, independent of an object's surviving SQL name.
use super::{Result, corrupt, functions};
use sqlx::{Postgres, Row, Transaction};

pub(super) async fn validate(tx: &mut Transaction<'_, Postgres>, version: u32) -> Result<()> {
    functions(tx, version).await?;
    columns(tx).await?;
    triggers(tx, version).await?;
    keys(tx, version).await?;
    indexes(tx).await?;
    checks(tx, version).await
}

async fn columns(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    for (table, expected) in [
        (
            "central_supervisor_actors",
            &[
                ("actor_id", "uuid", true),
                ("conversation_id", "uuid", true),
                ("revision", "bigint", true),
                ("policy_revision", "bigint", true),
                ("policy_bytes", "text", true),
                ("policy_digest", "text", true),
                ("approved_by", "uuid", true),
                ("approved_identity", "jsonb", true),
                ("current_reservation", "uuid", false),
                ("retained_operations", "bigint", true),
                ("retained_bytes", "bigint", true),
            ][..],
        ),
        (
            "central_supervisor_reservations",
            &[
                ("reservation_id", "uuid", true),
                ("actor_id", "uuid", true),
                ("reserve_operation_id", "uuid", true),
                ("policy_revision", "bigint", true),
                ("revision", "bigint", true),
                ("admission_actor_revision", "bigint", true),
                ("cancel_operation_id", "uuid", false),
                ("cancel_actor_revision", "bigint", false),
                ("state", "text", true),
                ("through_cursor", "bigint", true),
                ("batch_digest", "text", true),
                ("message_count", "smallint", true),
                ("body_bytes", "bigint", true),
            ][..],
        ),
        (
            "central_supervisor_mail_members",
            &[
                ("actor_id", "uuid", true),
                ("reservation_id", "uuid", true),
                ("ordinal", "smallint", true),
                ("message_id", "uuid", true),
                ("mailbox_key", "text", true),
                ("cursor", "bigint", true),
                ("payload_digest", "text", true),
            ][..],
        ),
        (
            "central_supervisor_operations",
            &[
                ("actor_id", "uuid", true),
                ("operation_id", "uuid", true),
                ("kind", "text", true),
                ("actor_revision", "bigint", true),
                ("initiating_user_id", "uuid", true),
                ("request_bytes", "text", true),
                ("request_digest", "text", true),
                ("result_bytes", "text", true),
                ("charged_bytes", "bigint", true),
            ][..],
        ),
    ] {
        let rows = sqlx::query("SELECT a.attname::text AS name,format_type(a.atttypid,a.atttypmod) AS kind,a.attnotnull,a.attgenerated::text,a.attidentity::text,a.atthasdef FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid WHERE a.attrelid=to_regclass($1) AND c.relkind='r' AND NOT c.relispartition AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum")
            .bind(table).fetch_all(&mut **tx).await?;
        if rows.len() != expected.len() {
            return Err(corrupt());
        }
        for (row, (name, kind, notnull)) in rows.iter().zip(expected) {
            if row.get::<String, _>("name") != *name
                || row.get::<String, _>("kind") != *kind
                || row.get::<bool, _>("attnotnull") != *notnull
                || !row.get::<String, _>("attgenerated").is_empty()
                || !row.get::<String, _>("attidentity").is_empty()
                || row.get::<bool, _>("atthasdef")
            {
                return Err(corrupt());
            }
        }
    }
    Ok(())
}

async fn triggers(tx: &mut Transaction<'_, Postgres>, version: u32) -> Result<()> {
    for (table, name, kind, function, deferred) in [
        (
            "mailbox_messages",
            "central_global_mail_row_guard",
            27,
            "guard_central_global_mail",
            false,
        ),
        (
            "mailbox_messages",
            "central_global_mail_truncate_guard",
            34,
            "guard_central_global_mail",
            false,
        ),
        (
            "mailbox_operations",
            "central_global_receipt_row_guard",
            27,
            "guard_central_global_mail",
            false,
        ),
        (
            "mailbox_operations",
            "central_global_receipt_truncate_guard",
            34,
            "guard_central_global_mail",
            false,
        ),
        (
            "central_supervisor_actors",
            "central_actor_row_guard",
            27,
            "guard_central_reservation_actor",
            false,
        ),
        (
            "central_supervisor_actors",
            "central_actor_truncate_guard",
            34,
            "guard_central_reservation_actor",
            false,
        ),
        (
            "central_supervisor_reservations",
            "central_reservation_row_guard",
            27,
            "guard_central_reservation_state",
            false,
        ),
        (
            "central_supervisor_reservations",
            "central_reservation_truncate_guard",
            34,
            "guard_central_reservation_state",
            false,
        ),
        (
            "central_supervisor_mail_members",
            "central_member_row_guard",
            27,
            "guard_central_reservation_immutable",
            false,
        ),
        (
            "central_supervisor_mail_members",
            "central_member_truncate_guard",
            34,
            "guard_central_reservation_immutable",
            false,
        ),
        (
            "central_supervisor_operations",
            "central_operation_row_guard",
            27,
            "guard_central_reservation_immutable",
            false,
        ),
        (
            "central_supervisor_operations",
            "central_operation_truncate_guard",
            34,
            "guard_central_reservation_immutable",
            false,
        ),
        (
            "central_supervisor_operations",
            "central_operation_consistency",
            5,
            "check_central_reservation_consistency",
            true,
        ),
        (
            "central_supervisor_actors",
            "central_actor_consistency",
            21,
            "check_central_reservation_consistency",
            true,
        ),
        (
            "central_supervisor_reservations",
            "central_reservation_consistency",
            21,
            "check_central_reservation_consistency",
            true,
        ),
        (
            "central_supervisor_mail_members",
            "central_member_consistency",
            5,
            "check_central_reservation_consistency",
            true,
        ),
    ]
    .into_iter()
    .chain(
        (version >= 49)
            .then_some([
                (
                    "central_supervisor_replies",
                    "central_supervisor_reply_consumption",
                    5,
                    "consume_central_supervisor_turn",
                    false,
                ),
                (
                    "central_session_generations",
                    "central_supervisor_delivery_consumption",
                    21,
                    "consume_central_supervisor_turn",
                    false,
                ),
            ])
            .into_iter()
            .flatten(),
    ) {
        let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_trigger WHERE tgrelid=to_regclass($1) AND tgname=$2 AND tgtype=$3 AND tgfoid=to_regprocedure($4) AND NOT tgisinternal AND tgenabled IN ('O','A') AND tgdeferrable=$5 AND tginitdeferred=$5 AND (tgconstraint<>0)=$5 AND tgnargs=0 AND tgqual IS NULL AND tgoldtable IS NULL AND tgnewtable IS NULL)")
            .bind(table).bind(name).bind(kind as i16).bind(format!("{function}()"))
            .bind(deferred).fetch_one(&mut **tx).await?;
        if !valid {
            return Err(corrupt());
        }
    }
    Ok(())
}

async fn keys(tx: &mut Transaction<'_, Postgres>, version: u32) -> Result<()> {
    // Compare full key shape, including the backing unique index's validity,
    // rather than trusting a name or merely the referenced relation.
    let actual: Vec<String> = sqlx::query_scalar(r#"SELECT c.conrelid::regclass::text||':'||c.contype::text||':'||array_to_string(ARRAY(SELECT a.attname::text FROM unnest(c.conkey) WITH ORDINALITY k(n,o) JOIN pg_attribute a ON a.attrelid=c.conrelid AND a.attnum=k.n ORDER BY k.o),',')||':'||CASE WHEN c.confrelid=0 THEN '' ELSE c.confrelid::regclass::text END||':'||COALESCE(array_to_string(ARRAY(SELECT a.attname::text FROM unnest(c.confkey) WITH ORDINALITY k(n,o) JOIN pg_attribute a ON a.attrelid=c.confrelid AND a.attnum=k.n ORDER BY k.o),','),'')||':'||c.condeferrable::text||':'||c.condeferred::text
        FROM pg_constraint c WHERE c.conrelid=ANY(ARRAY['central_supervisor_actors'::regclass,'central_supervisor_reservations'::regclass,'central_supervisor_mail_members'::regclass,'central_supervisor_operations'::regclass]) AND c.contype IN ('p','u','f') AND c.convalidated
        AND ((c.contype IN ('p','u') AND EXISTS(SELECT 1 FROM pg_index i WHERE i.indexrelid=c.conindid AND i.indrelid=c.conrelid AND i.indisvalid AND i.indisready AND i.indisunique AND i.indexprs IS NULL AND i.indpred IS NULL AND i.indnatts=i.indnkeyatts AND i.indnkeyatts=cardinality(c.conkey))) OR (c.contype='f' AND c.confupdtype='a' AND c.confdeltype='a' AND c.confmatchtype='s' AND (SELECT count(*)=4 AND bool_and(t.tgisinternal AND t.tgenabled IN ('O','A')) FROM pg_trigger t WHERE t.tgconstraint=c.oid)))"#)
        .fetch_all(&mut **tx).await?;
    let mut expected = vec![
        "central_supervisor_actors:p:actor_id:::false:false".to_string(),
        "central_supervisor_actors:u:conversation_id:::false:false".to_string(),
        "central_supervisor_reservations:p:reservation_id:::false:false".to_string(),
        "central_supervisor_reservations:u:actor_id,reservation_id:::false:false".to_string(),
        "central_supervisor_reservations:u:actor_id,reserve_operation_id:::false:false".to_string(),
        "central_supervisor_mail_members:p:reservation_id,ordinal:::false:false".to_string(),
        "central_supervisor_mail_members:u:reservation_id,message_id:::false:false".to_string(),
        "central_supervisor_mail_members:u:reservation_id,cursor:::false:false".to_string(),
        "central_supervisor_operations:p:actor_id,operation_id:::false:false".to_string(),
        "central_supervisor_operations:u:actor_id,actor_revision:::false:false".to_string(),
        "central_supervisor_actors:f:conversation_id:global_supervisor_conversation:conversation_id:false:false".to_string(),
        "central_supervisor_actors:f:approved_by:users:id:false:false".to_string(),
        "central_supervisor_actors:f:actor_id,current_reservation:central_supervisor_reservations:actor_id,reservation_id:false:false".to_string(),
        "central_supervisor_reservations:f:actor_id:central_supervisor_actors:actor_id:false:false".to_string(),
        "central_supervisor_reservations:f:actor_id,reserve_operation_id:central_supervisor_operations:actor_id,operation_id:true:true".to_string(),
        "central_supervisor_reservations:f:actor_id,cancel_operation_id:central_supervisor_operations:actor_id,operation_id:true:true".to_string(),
        "central_supervisor_mail_members:f:actor_id:central_supervisor_actors:actor_id:false:false".to_string(),
        "central_supervisor_mail_members:f:message_id:mailbox_messages:message_id:false:false".to_string(),
        "central_supervisor_mail_members:f:actor_id,reservation_id:central_supervisor_reservations:actor_id,reservation_id:false:false".to_string(),
        "central_supervisor_operations:f:actor_id:central_supervisor_actors:actor_id:false:false".to_string(),
        "central_supervisor_operations:f:initiating_user_id:users:id:false:false".to_string(),
    ];
    let mut actual = actual;
    actual.sort();
    if version >= 109 {
        for key in &mut expected {
            *key = key.replace(
                ":global_supervisor_conversation:",
                ":supervisor_conversation_ids:",
            );
        }
    }
    expected.sort();
    if actual != expected {
        return Err(corrupt());
    }
    Ok(())
}

async fn indexes(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    for (table, name, columns, options, unique, expression, predicate) in [
        (
            "central_supervisor_reservations",
            "central_supervisor_reservation_history",
            vec!["actor_id", "admission_actor_revision"],
            vec![0i16, 3],
            false,
            None,
            None,
        ),
        (
            "central_supervisor_reservations",
            "central_supervisor_one_active",
            vec!["actor_id"],
            vec![0i16],
            true,
            None,
            Some("(state = 'awaiting_prepared_plan'::text)"),
        ),
        (
            "central_supervisor_mail_members",
            "central_supervisor_member_message",
            vec!["actor_id", "message_id", "reservation_id"],
            vec![0i16, 0, 0],
            false,
            None,
            None,
        ),
        (
            "central_supervisor_mail_members",
            "central_supervisor_member_cursor",
            vec!["actor_id", "mailbox_key", "cursor", "reservation_id"],
            vec![0i16, 0, 0, 0],
            false,
            None,
            None,
        ),
        (
            "mailbox_operations",
            "mailbox_global_queued_original",
            vec![""],
            vec![0i16],
            true,
            Some("(response ->> 'message_id'::text)"),
            Some(
                "(((response ->> 'contract'::text) = 'human-supervisor-conversation-1'::text) AND ((response ->> 'state'::text) = 'queued'::text))",
            ),
        ),
    ] {
        let valid:bool=sqlx::query_scalar(r#"SELECT EXISTS(SELECT 1 FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid JOIN pg_am am ON am.oid=c.relam WHERE i.indexrelid=to_regclass($1) AND i.indrelid=to_regclass($2) AND i.indisvalid AND i.indisready AND i.indisunique=$3 AND am.amname='btree' AND i.indnatts=i.indnkeyatts AND ARRAY(SELECT unnest(i.indoption))=$4 AND pg_get_expr(i.indexprs,i.indrelid) IS NOT DISTINCT FROM $5 AND pg_get_expr(i.indpred,i.indrelid) IS NOT DISTINCT FROM $6 AND ARRAY(SELECT COALESCE(a.attname::text,'') FROM unnest(i.indkey) WITH ORDINALITY k(n,o) LEFT JOIN pg_attribute a ON a.attrelid=i.indrelid AND a.attnum=k.n ORDER BY k.o)=$7)"#)
            .bind(name).bind(table).bind(unique).bind(options).bind(expression).bind(predicate).bind(columns)
            .fetch_one(&mut **tx).await?;
        if !valid {
            return Err(corrupt());
        }
    }
    Ok(())
}

async fn checks(tx: &mut Transaction<'_, Postgres>, version: u32) -> Result<()> {
    // Full canonical CHECK definitions are pinned by the disposable fresh37
    // qualification, including casts and boolean grouping, not name-only probes.
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM pg_constraint WHERE conrelid=ANY(ARRAY['central_supervisor_actors'::regclass,'central_supervisor_reservations'::regclass,'central_supervisor_mail_members'::regclass,'central_supervisor_operations'::regclass]) AND contype='c'")
        .fetch_one(&mut **tx).await?;
    if count != CHECKS.len() as i64 {
        return Err(corrupt());
    }
    for (table, name, expression) in CHECKS {
        let updated;
        let expression: &str = if version >= 49
            && *name == "central_supervisor_reservations_state_check"
        {
            "(state = ANY (ARRAY['awaiting_prepared_plan'::text, 'cancelled_before_plan'::text, 'consumed'::text]))"
        } else if version >= 49 && *name == "central_supervisor_reservations_check2" {
            updated = expression.replace(" OR ((state = 'cancelled_before_plan'::text)", " OR ((state = 'consumed'::text) AND (revision = 2) AND (cancel_operation_id IS NULL) AND (cancel_actor_revision IS NULL)) OR ((state = 'cancelled_before_plan'::text)");
            &updated
        } else {
            expression
        };
        let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_constraint WHERE conrelid=to_regclass($1) AND contype='c' AND convalidated AND NOT condeferrable AND conname=$2 AND pg_get_expr(conbin,conrelid)=$3)")
            .bind(table).bind(name).bind(expression).fetch_one(&mut **tx).await?;
        if !valid {
            return Err(corrupt());
        }
    }
    Ok(())
}

const CHECKS: &[(&str, &str, &str)] = &[
    (
        "central_supervisor_actors",
        "central_supervisor_actors_actor_id_check",
        "(actor_id <> '00000000-0000-0000-0000-000000000000'::uuid)",
    ),
    (
        "central_supervisor_actors",
        "central_supervisor_actors_check",
        "(policy_digest = encode(sha256(convert_to(policy_bytes, 'UTF8'::name)), 'hex'::text))",
    ),
    (
        "central_supervisor_actors",
        "central_supervisor_actors_check1",
        "((revision = retained_operations) AND (policy_revision <= revision))",
    ),
    (
        "central_supervisor_actors",
        "central_supervisor_actors_check2",
        "(((jsonb_typeof((policy_bytes)::jsonb) = 'object'::text) AND ((policy_bytes)::jsonb = jsonb_build_object('schema_version', 1, 'kind', 'global_supervisor_reservation', 'actor_id', (actor_id)::text, 'conversation_id', (conversation_id)::text, 'enabled', ((policy_bytes)::jsonb -> 'enabled'::text))) AND (jsonb_typeof(((policy_bytes)::jsonb -> 'enabled'::text)) = 'boolean'::text)) IS TRUE)",
    ),
    (
        "central_supervisor_actors",
        "central_supervisor_actors_check3",
        "(((jsonb_typeof(approved_identity) = 'object'::text) AND ((approved_identity ->> 'id'::text) = (approved_by)::text) AND ((approved_identity ->> 'role'::text) = 'owner'::text) AND (jsonb_typeof((approved_identity -> 'email'::text)) = 'string'::text) AND (jsonb_typeof((approved_identity -> 'issuer'::text)) = ANY (ARRAY['string'::text, 'null'::text])) AND (jsonb_typeof((approved_identity -> 'subject'::text)) = ANY (ARRAY['string'::text, 'null'::text]))) IS TRUE)",
    ),
    (
        "central_supervisor_actors",
        "central_supervisor_actors_check4",
        "((current_reservation IS NULL) OR ((retained_operations <= 4095) AND (retained_bytes <= 66977792)))",
    ),
    (
        "central_supervisor_actors",
        "central_supervisor_actors_policy_bytes_check",
        "((octet_length(policy_bytes) >= 1) AND (octet_length(policy_bytes) <= 4096))",
    ),
    (
        "central_supervisor_actors",
        "central_supervisor_actors_policy_revision_check",
        "(policy_revision > 0)",
    ),
    (
        "central_supervisor_actors",
        "central_supervisor_actors_retained_bytes_check",
        "((retained_bytes >= 1) AND (retained_bytes <= 67108864))",
    ),
    (
        "central_supervisor_actors",
        "central_supervisor_actors_retained_operations_check",
        "((retained_operations >= 1) AND (retained_operations <= 4096))",
    ),
    (
        "central_supervisor_actors",
        "central_supervisor_actors_revision_check",
        "(revision > 0)",
    ),
    (
        "central_supervisor_mail_members",
        "central_supervisor_mail_members_cursor_check",
        "(cursor > 0)",
    ),
    (
        "central_supervisor_mail_members",
        "central_supervisor_mail_members_mailbox_key_check",
        "(mailbox_key ~ '^[0-9a-f]{64}$'::text)",
    ),
    (
        "central_supervisor_mail_members",
        "central_supervisor_mail_members_ordinal_check",
        "((ordinal >= 0) AND (ordinal <= 15))",
    ),
    (
        "central_supervisor_mail_members",
        "central_supervisor_mail_members_payload_digest_check",
        "(payload_digest ~ '^[0-9a-f]{64}$'::text)",
    ),
    (
        "central_supervisor_operations",
        "central_supervisor_operations_actor_revision_check",
        "((actor_revision >= 1) AND (actor_revision <= 4096))",
    ),
    (
        "central_supervisor_operations",
        "central_supervisor_operations_check",
        "(request_digest = encode(sha256(convert_to(request_bytes, 'UTF8'::name)), 'hex'::text))",
    ),
    (
        "central_supervisor_operations",
        "central_supervisor_operations_check1",
        "((charged_bytes >= ((octet_length(request_bytes) + octet_length(result_bytes)) + 8192)) AND (charged_bytes <= 131072))",
    ),
    (
        "central_supervisor_operations",
        "central_supervisor_operations_check2",
        "(((jsonb_typeof((request_bytes)::jsonb) = 'object'::text) AND (((request_bytes)::jsonb ->> 'contract'::text) = 'central-reservation-1'::text) AND (((request_bytes)::jsonb ->> 'kind'::text) = kind) AND (((request_bytes)::jsonb #>> '{request,actor_id}'::text[]) = (actor_id)::text) AND (((request_bytes)::jsonb #>> '{request,operation_id}'::text[]) = (operation_id)::text) AND (((request_bytes)::jsonb #>> '{owner,id}'::text[]) = (initiating_user_id)::text) AND (((request_bytes)::jsonb #>> '{owner,role}'::text[]) = 'owner'::text)) IS TRUE)",
    ),
    (
        "central_supervisor_operations",
        "central_supervisor_operations_check3",
        "(((jsonb_typeof((result_bytes)::jsonb) = 'object'::text) AND (((result_bytes)::jsonb ->> 'contract'::text) = 'central-reservation-1'::text) AND (((result_bytes)::jsonb ->> 'actor_id'::text) = (actor_id)::text) AND (((result_bytes)::jsonb ->> 'operation_id'::text) = (operation_id)::text) AND (((result_bytes)::jsonb ->> 'initiating_user_id'::text) = (initiating_user_id)::text) AND (((result_bytes)::jsonb ->> 'request_digest'::text) = request_digest) AND (((result_bytes)::jsonb -> 'actor_revision'::text) = to_jsonb(actor_revision)) AND (((kind = 'configure'::text) AND (((result_bytes)::jsonb ->> 'disposition'::text) = ANY (ARRAY['configured'::text, 'not_configured'::text]))) OR ((kind = 'reserve'::text) AND (((result_bytes)::jsonb ->> 'disposition'::text) = ANY (ARRAY['reserved'::text, 'not_reserved'::text]))) OR ((kind = 'cancel'::text) AND (((result_bytes)::jsonb ->> 'disposition'::text) = ANY (ARRAY['cancelled_before_plan'::text, 'not_cancelled'::text]))))) IS TRUE)",
    ),
    (
        "central_supervisor_operations",
        "central_supervisor_operations_kind_check",
        "(kind = ANY (ARRAY['configure'::text, 'reserve'::text, 'cancel'::text]))",
    ),
    (
        "central_supervisor_operations",
        "central_supervisor_operations_operation_id_check",
        "(operation_id <> '00000000-0000-0000-0000-000000000000'::uuid)",
    ),
    (
        "central_supervisor_operations",
        "central_supervisor_operations_request_bytes_check",
        "((octet_length(request_bytes) >= 1) AND (octet_length(request_bytes) <= 16384))",
    ),
    (
        "central_supervisor_operations",
        "central_supervisor_operations_result_bytes_check",
        "((octet_length(result_bytes) >= 1) AND (octet_length(result_bytes) <= 65536))",
    ),
    (
        "central_supervisor_reservations",
        "central_supervisor_reservations_batch_digest_check",
        "(batch_digest ~ '^[0-9a-f]{64}$'::text)",
    ),
    (
        "central_supervisor_reservations",
        "central_supervisor_reservations_body_bytes_check",
        "((body_bytes >= 1) AND (body_bytes <= 65536))",
    ),
    (
        "central_supervisor_reservations",
        "central_supervisor_reservations_check",
        "((reserve_operation_id <> reservation_id) AND (reserve_operation_id <> '00000000-0000-0000-0000-000000000000'::uuid))",
    ),
    (
        "central_supervisor_reservations",
        "central_supervisor_reservations_check1",
        "((admission_actor_revision > 0) AND (policy_revision <= admission_actor_revision))",
    ),
    (
        "central_supervisor_reservations",
        "central_supervisor_reservations_check2",
        "(((state = 'awaiting_prepared_plan'::text) AND (revision = 1) AND (cancel_operation_id IS NULL) AND (cancel_actor_revision IS NULL)) OR ((state = 'cancelled_before_plan'::text) AND (revision = 2) AND (cancel_operation_id IS NOT NULL) AND (cancel_operation_id <> '00000000-0000-0000-0000-000000000000'::uuid) AND (cancel_operation_id <> reserve_operation_id) AND (cancel_actor_revision IS NOT NULL) AND (cancel_actor_revision > admission_actor_revision)))",
    ),
    (
        "central_supervisor_reservations",
        "central_supervisor_reservations_message_count_check",
        "((message_count >= 1) AND (message_count <= 16))",
    ),
    (
        "central_supervisor_reservations",
        "central_supervisor_reservations_policy_revision_check",
        "(policy_revision > 0)",
    ),
    (
        "central_supervisor_reservations",
        "central_supervisor_reservations_reservation_id_check",
        "(reservation_id <> '00000000-0000-0000-0000-000000000000'::uuid)",
    ),
    (
        "central_supervisor_reservations",
        "central_supervisor_reservations_revision_check",
        "(revision > 0)",
    ),
    (
        "central_supervisor_reservations",
        "central_supervisor_reservations_state_check",
        "(state = ANY (ARRAY['awaiting_prepared_plan'::text, 'cancelled_before_plan'::text]))",
    ),
    (
        "central_supervisor_reservations",
        "central_supervisor_reservations_through_cursor_check",
        "(through_cursor > 0)",
    ),
];

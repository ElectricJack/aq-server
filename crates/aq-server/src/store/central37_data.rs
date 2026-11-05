//! Read-only revalidation of retained facts, including faults introduced while
//! guards were temporarily disabled. No UPDATE, backfill or implicit repair.
use super::{Result, corrupt, function_body};
use sqlx::{Postgres, Transaction};

pub(super) async fn validate(tx: &mut Transaction<'_, Postgres>, version: u32) -> Result<()> {
    let valid: bool = sqlx::query_scalar(r#"SELECT
      (SELECT count(*)=1 AND bool_and(singleton AND revision=1 AND conversation_id<>'00000000-0000-0000-0000-000000000000'::uuid) FROM global_supervisor_conversation)
      AND NOT EXISTS(SELECT 1 FROM mailbox_messages m WHERE (m.mailbox_key=central_global_mailbox_key() OR m.recipient->>'kind'='global_supervisor') AND NOT central_global_mail_source_valid(m.message_id))
      AND NOT EXISTS(SELECT 1 FROM mailbox_operations o WHERE o.response->>'contract'='human-supervisor-conversation-1' AND o.response->>'state'='queued' AND NOT EXISTS(SELECT 1 FROM mailbox_messages m WHERE m.message_id::text=o.response->>'message_id' AND central_global_mail_source_valid(m.message_id)))
      AND COALESCE((SELECT h.next_cursor=(SELECT COALESCE(max(m.cursor),0)+1 FROM mailbox_messages m WHERE m.mailbox_key=central_global_mailbox_key()) FROM mailbox_heads h WHERE h.mailbox_key=central_global_mailbox_key()),NOT EXISTS(SELECT 1 FROM mailbox_messages m WHERE m.mailbox_key=central_global_mailbox_key()))
      AND NOT EXISTS(SELECT 1 FROM central_supervisor_actors a WHERE NOT EXISTS(SELECT 1 FROM global_supervisor_conversation g WHERE g.conversation_id=a.conversation_id) OR NOT EXISTS(SELECT 1 FROM users u WHERE u.id=a.approved_by)
        OR (a.current_reservation IS NOT NULL AND NOT EXISTS(SELECT 1 FROM central_supervisor_reservations r WHERE r.actor_id=a.actor_id AND r.reservation_id=a.current_reservation)))
      AND NOT EXISTS(SELECT 1 FROM central_supervisor_reservations r WHERE NOT EXISTS(SELECT 1 FROM central_supervisor_actors a WHERE a.actor_id=r.actor_id)
        OR NOT EXISTS(SELECT 1 FROM central_supervisor_operations o WHERE o.actor_id=r.actor_id AND o.operation_id=r.reserve_operation_id)
        OR (r.cancel_operation_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM central_supervisor_operations o WHERE o.actor_id=r.actor_id AND o.operation_id=r.cancel_operation_id)))
      AND NOT EXISTS(SELECT 1 FROM central_supervisor_mail_members b WHERE NOT EXISTS(SELECT 1 FROM central_supervisor_actors a WHERE a.actor_id=b.actor_id)
        OR NOT EXISTS(SELECT 1 FROM central_supervisor_reservations r WHERE r.actor_id=b.actor_id AND r.reservation_id=b.reservation_id)
        OR NOT EXISTS(SELECT 1 FROM mailbox_messages m WHERE m.message_id=b.message_id))
      AND NOT EXISTS(SELECT 1 FROM central_supervisor_operations o WHERE NOT EXISTS(SELECT 1 FROM central_supervisor_actors a WHERE a.actor_id=o.actor_id)
        OR NOT EXISTS(SELECT 1 FROM users u WHERE u.id=o.initiating_user_id))
    "#).fetch_one(&mut **tx).await?;
    if !valid {
        return Err(corrupt());
    }
    // Explicit antijoins above also cover child rows outside every retained
    // actor/reservation loop after FK triggers were disabled then re-enabled.
    // convalidated is not evidence that such historical rows were rechecked.
    // Replay the exact frozen *read-only* deferred consistency predicates over
    // retained actors/reservations. This is not CREATE FUNCTION, a trigger fire,
    // or a schema repair. The same bounded member/source/operation joins are used
    // for post-disable corruption and deferred transition validation.
    sqlx::raw_sql(&retained_check_sql(version)?)
        .execute(&mut **tx)
        .await
        .map_err(|_| corrupt())?;
    Ok(())
}

fn retained_check_sql(version: u32) -> Result<String> {
    let source = function_body("check_central_reservation_consistency", version)?;
    let (declarations, body) = source.split_once("BEGIN\n").ok_or_else(corrupt)?;
    let body = body.strip_suffix("END ").ok_or_else(corrupt)?;
    if body.matches("RETURN NULL;").count() != 2 {
        return Err(corrupt());
    }
    let body = body.replace("RETURN NULL;", "CONTINUE;");
    Ok(format!(
        r#"DO $central37_startup$
{declarations}
 NEW RECORD; TG_TABLE_NAME TEXT;
BEGIN
 FOR NEW IN
   SELECT actor_id,NULL::uuid AS reservation_id,'central_supervisor_actors'::text AS relation FROM central_supervisor_actors
   UNION ALL SELECT actor_id,reservation_id,'central_supervisor_reservations'::text FROM central_supervisor_reservations
 LOOP
   TG_TABLE_NAME:=NEW.relation;
{body}
 END LOOP;
END $central37_startup$;"#
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_checker_reuses_frozen_predicates_without_writing_or_returning() {
        let sql = retained_check_sql(37).unwrap();
        assert!(sql.contains("central original reserve operation proof is inconsistent"));
        assert!(sql.contains("central original cancel operation proof is inconsistent"));
        assert!(!sql.contains("RETURN NULL;"));
        for command in [
            "UPDATE ",
            "INSERT ",
            "DELETE ",
            "TRUNCATE ",
            "CREATE ",
            "ALTER ",
        ] {
            assert!(!sql.contains(command), "{command}");
        }
    }
}

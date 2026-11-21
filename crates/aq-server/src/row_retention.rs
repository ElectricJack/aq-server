//! DEP6 row retention. Each statement deletes at most `BATCH` rows; dependent
//! facts/receipts are removed before their parents. See docs/operations/row-retention.md
//! for the age, authority and idempotency predicates for every table.
pub use aq_row_retention::*;

pub async fn run(state: crate::AppState, mut stop: tokio::sync::watch::Receiver<bool>) {
    let mut notices = tokio::time::interval(std::time::Duration::from_secs(60));
    notices.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut interval = tokio::time::interval(INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if *stop.borrow() {
            break;
        }
        tokio::select! {
            _=notices.tick(), if cfg!(feature = "delivery_playbook") => {
                #[cfg(feature = "delivery_playbook")]
                if let Err(e) = aq_playbook_domain::scheduling_notices::notify(&state.store.pool, (state.clock)().timestamp_millis(), crate::agent_access::supervisor_conversation::system_message_in).await {
                    tracing::warn!(error=?e, "scheduler supervisor escalation deferred");
                }
            },
            changed=stop.changed()=>{if changed.is_err()||*stop.borrow(){break;}},
            _=interval.tick()=>{
                match collect(&state.store.pool,&state.agent_access,(state.clock)().timestamp_millis(),BATCH).await {
                    Ok(report)=>for (table,rows_deleted) in report {tracing::info!(table,rows_deleted,"row retention completed");},
                    Err(_)=>tracing::warn!("row retention deferred; retained rows will retry"),
                }
                let log=presence_operations(&state.store.pool,(state.clock)().timestamp_millis(),&state.config.workers.heartbeat_log).await;
                match log {Ok(rows_deleted)=>tracing::info!(rows_deleted,"heartbeat log retention completed"),Err(error)=>tracing::warn!(%error,"heartbeat log retention deferred")}
                match registration_history(&state.store.pool,(state.clock)().timestamp_millis(),&state.config.workers.registration_history).await {Ok(rows_deleted)=>tracing::info!(rows_deleted,"registration history retention completed"),Err(error)=>tracing::warn!(%error,"registration history retention deferred")}
                // R6: a held task closed or retired in Graph is no longer held.
                let swept=aq_machine_control::holds::sweep(&state.store.pool,&state.agent_access,(state.clock)()).await;
                if let Err(e)=swept {tracing::warn!(error=?e,"task hold sweep deferred");}
            }
        }
    }
}

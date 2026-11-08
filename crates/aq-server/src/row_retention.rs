//! DEP6 row retention. Each statement deletes at most `BATCH` rows; dependent
//! facts/receipts are removed before their parents. See docs/operations/row-retention.md
//! for the age, authority and idempotency predicates for every table.
pub use aq_row_retention::*;

pub async fn run(state: crate::AppState, mut stop: tokio::sync::watch::Receiver<bool>) {
    let mut interval = tokio::time::interval(INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if *stop.borrow() {
            break;
        }
        tokio::select! {
            changed=stop.changed()=>{if changed.is_err()||*stop.borrow(){break;}},
            _=interval.tick()=>{
                match collect(&state.store.pool,&state.agent_access,(state.clock)().timestamp_millis(),BATCH).await {
                    Ok(report)=>for (table,rows_deleted) in report {tracing::info!(table,rows_deleted,"row retention completed");},
                    Err(_)=>tracing::warn!("row retention deferred; retained rows will retry"),
                }
                // R6: a held task closed or retired in Graph is no longer held.
                let swept=aq_machine_control::holds::sweep(&state.store.pool,&state.agent_access,(state.clock)()).await;
                if let Err(e)=swept {tracing::warn!(error=?e,"task hold sweep deferred");}
            }
        }
    }
}

//! A poll's early gates, answered from a recent result while it says "go on".
//! Occupancy validates every held attempt's stop proof in SQL, which is too
//! dear to repeat for each idle slot of a node on every poll. Only "free" and
//! "no hold" are reused, for at most `TTL`; "occupied" and "held" are read
//! again each time, so a released slot is seen at once. Nothing here decides:
//! publication reads the current rows under the node lock and the SQL slot
//! fence refuses a second occupant, so a stale "free" costs one assembly that
//! publishes nothing.
use crate::{Error, slots::SlotView};
use sqlx::PgConnection;
use std::{collections::HashMap, time::Duration, time::Instant};
use tokio::sync::Mutex;
use uuid::Uuid;

const TTL: Duration = Duration::from_secs(1);
type Recent<T> = Mutex<Option<HashMap<Uuid, (Instant, T)>>>;
static VIEWS: Recent<SlotView> = Mutex::const_new(None);
static HOLDS: Recent<Option<&'static str>> = Mutex::const_new(None);

/// One reader at a time computes; the others of the same instant reuse it.
macro_rules! recent {
    ($cache:expr, $node:expr, $reusable:expr, $compute:expr) => {{
        let mut cache = $cache.lock().await;
        let entries = cache.get_or_insert_with(HashMap::new);
        match entries.get(&$node) {
            Some((at, value)) if at.elapsed() < TTL => value.clone(),
            _ => {
                let value = $compute;
                entries.retain(|_, (at, _)| at.elapsed() < TTL);
                if $reusable(&value) {
                    entries.insert($node, (Instant::now(), value.clone()));
                } else {
                    entries.remove(&$node);
                }
                value
            }
        }
    }};
}
/// `slots::poll_free` over the recent view: a poll's early occupancy gate.
pub async fn poll_free(conn: &mut PgConnection, node: Uuid, slot: u32) -> Result<bool, Error> {
    let free = |view: &SlotView| view.free(slot);
    let view = recent!(VIEWS, node, free, crate::slots::view(conn, node).await?);
    Ok(view.free(slot) && crate::slots::registered(conn, node, slot).await?)
}
/// `offer_hold`, at most `TTL` old: a poll's early gate, rechecked at publication.
pub async fn offer_hold(
    conn: &mut PgConnection,
    node: Uuid,
) -> Result<Option<&'static str>, Error> {
    let none = |hold: &Option<&'static str>| hold.is_none();
    Ok(recent!(
        HOLDS,
        node,
        none,
        crate::offer_hold(conn, node).await?
    ))
}
/// An offer was published or an attempt released: the next gate reads again.
pub async fn forget(node: Uuid) {
    if let Some(entries) = VIEWS.lock().await.as_mut() {
        entries.remove(&node);
    }
    if let Some(entries) = HOLDS.lock().await.as_mut() {
        entries.remove(&node);
    }
}

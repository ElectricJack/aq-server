//! Fair multi-project scheduling (worker spec §8.2): configured priority plus
//! aging, with a hard starvation bound. Ordering is pure; the last-service
//! record is the only durable input.

pub use aq_profiled_state::fairness::{Candidate, order, record, recorded, served};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use uuid::Uuid;

    #[tokio::test]
    async fn service_record_never_moves_backwards() {
        let db = crate::test_support::TestDb::create().await;
        let store = db.store().await;
        let project = Uuid::new_v4();
        record(&store.pool, project, 20).await.unwrap();
        record(&store.pool, project, 10).await.unwrap();
        assert_eq!(
            served(&store.pool).await.unwrap(),
            BTreeMap::from([(project, 20)])
        );
    }
}

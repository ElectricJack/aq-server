//! Disposable old-writer qualification helper, not a workflow driver.
use aq_playbooks_postgres::PostgresStorage;
use aq_playbooks_runner::{CatalogKind, CatalogQuery, Engine, Storage};
fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().expect("mode");
    let namespace = args.next().expect("namespace");
    let url =
        std::env::var("AQ_PLAYBOOKS_TEST_DATABASE_URL").expect("explicit disposable database");
    let store = PostgresStorage::connect(&url).unwrap();
    if mode == "migrate" {
        let state = store.load(&namespace).unwrap();
        assert_eq!(state.state.sequence, 99);
        println!(
            "{}",
            serde_json::json!({"stage":"migrated","sequence":state.state.sequence,"state_revision":state.revision})
        );
    } else {
        assert_eq!(mode, "inspect");
        let engine = Engine::new(store, &namespace, "new-catalog-owner").unwrap();
        assert_eq!(engine.snapshot().unwrap().sequence, 99);
        let page = engine
            .inspect_catalog(&CatalogQuery::first(CatalogKind::Definitions, 25))
            .unwrap();
        assert!(page.items.is_empty());
        assert!(page.stamp.incarnation >= 2);
        engine.release().unwrap();
        println!("{}", serde_json::to_string(&page).unwrap());
    }
}

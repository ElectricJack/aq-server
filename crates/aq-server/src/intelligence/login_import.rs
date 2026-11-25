//! Operator-facing login file imports use the versioned credential store.
pub use aq_login_store::import::{
    LoginFile, is_claude_setup_token, normalize_claude_token, read_source,
};

pub async fn import(
    store: &crate::store::Store,
    loaded: &super::config::LoadedRegistry,
    file: LoginFile<'_>,
    now_ms: i64,
) -> anyhow::Result<i64> {
    aq_login_store::import::import(&store.pool, loaded, file, now_ms).await
}

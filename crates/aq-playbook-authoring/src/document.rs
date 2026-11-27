//! A built-in source whose document is a Settings value (spec 0013 §7; the
//! delivery document of spec 0016 §8): saving the document starts authoring,
//! and a slow pass catches up a save that was missed.
use crate::{Server, SourceRecord, author, author_missed, ensure_builtin};

/// The source's document is empty (or unreadable): there is nothing to author.
async fn no_document<S: Server>(s: &S, record: &SourceRecord) -> bool {
    s.markdown(record)
        .await
        .map_or(true, |(markdown, _)| markdown.trim().is_empty())
}

/// Authors the document of `record` after a save of it: writes the source's
/// record (the same record changes nothing), then starts a run. Nothing starts
/// for an empty document or when nothing the agent would see changed. A server
/// without an open playbook engine only logs: the save has succeeded.
pub async fn author_document<S: Server>(s: &S, record: &SourceRecord) {
    if let Err(error) = ensure_builtin(s.as_ref(), record).await {
        tracing::warn!(source = %record.id, %error, "the source record was not written");
    }
    if no_document(s, record).await {
        return;
    }
    if let Err(error) = author(s, &record.id).await {
        tracing::info!(source = %record.id, %error, "the document was not authored");
    }
}

/// A pass of the slow catch-up ([`crate::serve`]): authors the document of
/// `record` unless its newest run was already for the document as it is (a
/// missed save is caught up). The engine may not be open yet; that is only
/// logged, and the next pass tries again.
pub async fn catch_up<S: Server>(s: &S, record: &SourceRecord) {
    if ensure_builtin(s.as_ref(), record).await.is_err() || no_document(s, record).await {
        return;
    }
    if let Err(error) = author_missed(s, &record.id).await {
        tracing::info!(source = %record.id, %error, "the document is not authored yet");
    }
}

/// The Settings route of the delivery document (`/api/settings/delivery-document`).
const DELIVERY_DOCUMENT: &str = "/delivery-document";

/// A Settings write to `path` succeeded: authors the delivery document when it
/// is the one saved.
pub async fn author_saved<S: Server>(s: &S, path: &str) {
    // A closed engine: the slow catch-up authors the save once it is open.
    if path.ends_with(DELIVERY_DOCUMENT)
        && let Some(record) = s.delivery()
    {
        author_document(s, &record).await;
    }
}

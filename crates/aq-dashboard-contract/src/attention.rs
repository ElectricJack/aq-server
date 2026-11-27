//! The complete "Needs you" list behind View All, and the rule for clearing an entry.
//!
//! The list is the overview's attention list (same sources, same filtering), so the
//! overview count and this list agree by construction. Clearing is a per-user
//! notification state: it never resolves, approves or cancels the underlying record.
use crate::{AttentionItem, AttentionKind, Overview, SourceObservation, Total};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Longest attention id the server accepts or stores.
pub const MAX_ID_BYTES: usize = 256;
pub const DEFAULT_LIMIT: usize = 25;
pub const MAX_LIMIT: usize = 100;
// New owner attention kinds sort after the established priorities. Keep this
// rank valid in cursors too, including when integrating newer owner variants.
const OTHER_KIND_RANK: u8 = 7;
/// Notification families eligible for dismissal. The server must additionally
/// check current revisions, affected kinds, authorization and unresolved gates.
const DISMISSIBLE_PREFIXES: [&str; 4] = ["result:", "internal_work:", "operation:", "worker:"];

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_BYTES && id.bytes().all(|b| b.is_ascii_graphic())
}
/// Whether an id belongs to a dismissible notification family. Authorization,
/// subject revisions and open gates still require current server checks.
pub fn dismissible(id: &str) -> bool {
    valid_id(id) && DISMISSIBLE_PREFIXES.iter().any(|p| id.starts_with(p))
}
/// Invalid request revision or a subject that changed since it was read.
#[derive(Debug, PartialEq, Eq)]
pub enum RevisionError {
    Invalid,
    Changed,
}

pub fn check_revision(expected: &str, current: Option<&str>) -> Result<(), RevisionError> {
    if expected.is_empty()
        || expected.len() > 256
        || !expected.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(RevisionError::Invalid);
    }
    if current.is_some_and(|revision| revision != expected) {
        return Err(RevisionError::Changed);
    }
    Ok(())
}

/// Filter per-principal, revision-pinned dismissals while protecting open gates.
pub fn filter_dismissed(
    data: &mut Overview,
    cleared: &std::collections::BTreeSet<(String, String)>,
    gates: &std::collections::BTreeSet<Uuid>,
    protected: &std::collections::BTreeSet<String>,
) {
    let before = data.attention.len();
    for item in &mut data.attention {
        item.dismissible = dismissible(&item.id)
            && !matches!(item.kind, AttentionKind::Decision | AttentionKind::Question)
            && !matches!(item.subject.kind.as_str(), "decision" | "question")
            && !gates.contains(&item.subject.id)
            && !protected.contains(&item.id);
    }
    data.attention.retain(|item| {
        let revision = item
            .subject
            .revision
            .as_ref()
            .and_then(|r| serde_json::to_value(r).ok())
            .and_then(|r| r.as_str().map(str::to_owned));
        !(item.dismissible && revision.is_some_and(|r| cleared.contains(&(item.id.clone(), r))))
    });
    if let Some(count) = &mut data.counts.needs_you.value {
        *count = count.saturating_sub((before - data.attention.len()) as u32);
    }
}

impl AttentionKind {
    pub fn parse(name: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(name.into())).ok()
    }
    /// Display order: what blocks the owner first.
    fn rank(&self) -> u8 {
        [
            Self::Decision,
            Self::Question,
            Self::UncertainOperation,
            Self::Blocked,
            Self::Failed,
            Self::InternalWork,
            Self::WorkerOffline,
        ]
        .iter()
        .position(|kind| kind == self)
        .map_or(OTHER_KIND_RANK, |rank| rank as u8)
    }
}

/// One page of the full attention list, with the coverage of every source behind it.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttentionPage {
    pub items: Vec<AttentionItem>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
    /// Entries matching the filter. Unknown, not zero, while an owner is unavailable.
    pub total: Total,
    /// Per-source coverage, as in the overview, so a partial list is not read as complete.
    pub sources: Vec<SourceObservation>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DismissAttention {
    pub expected_revision: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttentionDismissal {
    pub attention_id: String,
    /// Always true on success; the entry stays cleared across refreshes.
    pub dismissed: bool,
    /// The source revision whose notification was dismissed.
    pub expected_revision: String,
    /// Original timestamp, unchanged on replay.
    pub dismissed_at_ms: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PageError {
    Cursor,
}
fn key(item: &AttentionItem) -> (u8, &str) {
    (item.kind.rank(), item.id.as_str())
}
fn encode(item: &AttentionItem) -> String {
    let (rank, id) = key(item);
    format!("{rank}:{id}")
}
fn decode(cursor: &str) -> Result<(u8, &str), PageError> {
    let (rank, id) = cursor.split_once(':').ok_or(PageError::Cursor)?;
    let rank = rank.parse::<u8>().map_err(|_| PageError::Cursor)?;
    if rank > OTHER_KIND_RANK || !valid_id(id) {
        return Err(PageError::Cursor);
    }
    Ok((rank, id))
}
/// Filter, order and slice. The cursor is the last returned entry's sort key, so a
/// page stays correct when entries are resolved or cleared between requests.
/// Returns the page and the number of entries matching the filter.
pub fn page(
    mut items: Vec<AttentionItem>,
    kind: Option<&AttentionKind>,
    after: Option<&str>,
    limit: usize,
) -> Result<(Vec<AttentionItem>, Option<String>, usize), PageError> {
    let after = after.map(decode).transpose()?;
    items.retain(|i| kind.is_none_or(|k| *k == i.kind));
    items.sort_by(|a, b| key(a).cmp(&key(b)));
    items.dedup_by(|a, b| a.id == b.id);
    let matching = items.len();
    let start = after.map_or(0, |after| items.partition_point(|i| key(i) <= after));
    let rest = items.split_off(start);
    let mut page: Vec<AttentionItem> = rest.into_iter().take(limit + 1).collect();
    let next = (page.len() > limit).then(|| {
        page.truncate(limit);
        page.last().map(encode)
    });
    Ok((page, next.flatten(), matching))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate as contract;
    use crate::{EntityRef, Revision, overview};
    use uuid::Uuid;

    fn item(kind: AttentionKind, id: &str) -> AttentionItem {
        let mut item = overview::attention(
            kind,
            EntityRef {
                kind: "x".into(),
                id: Uuid::nil(),
                revision: None,
            },
            None,
            overview::AttentionDescription {
                title: "t",
                reason: "r",
                since: None,
            },
        );
        item.id = id.into();
        item
    }
    fn ids(items: &[AttentionItem]) -> Vec<&str> {
        items.iter().map(|i| i.id.as_str()).collect()
    }

    #[test]
    fn only_informational_entries_can_be_cleared() {
        assert!(dismissible("result:abc:def"));
        assert!(dismissible("internal_work:abc"));
        assert!(dismissible("operation:abc:def"));
        assert!(dismissible("worker:abc:def"));
        for id in [
            "decision:abc:def",
            "question:abc",
            "blocked:abc",
            "",
            "result: with space",
        ] {
            assert!(!dismissible(id), "{id}");
        }
        assert!(!dismissible(&format!(
            "result:{}",
            "a".repeat(MAX_ID_BYTES)
        )));
    }

    #[test]
    fn dismissal_contract_requires_the_revision_and_has_a_stable_record() {
        let request: DismissAttention =
            serde_json::from_str(r#"{"expected_revision":"12"}"#).unwrap();
        assert_eq!(request.expected_revision, "12");
        assert!(
            serde_json::from_str::<DismissAttention>(r#"{"attention_id":"internal_work:x"}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<DismissAttention>("{}").is_err());
        let record = AttentionDismissal {
            attention_id: "internal_work:x".into(),
            expected_revision: "12".into(),
            dismissed: true,
            dismissed_at_ms: 2000,
        };
        let serialized = serde_json::to_value(&record).unwrap();
        assert_eq!(serialized["dismissed_at_ms"], 2000);
        assert_eq!(
            serde_json::from_value::<AttentionDismissal>(serialized).unwrap(),
            record
        );
    }

    #[test]
    fn pages_cover_every_entry_once_in_a_stable_order() {
        let all = vec![
            item(AttentionKind::InternalWork, "internal_work:2"),
            item(AttentionKind::Decision, "decision:b"),
            item(AttentionKind::Failed, "result:1"),
            item(AttentionKind::Decision, "decision:a"),
            item(AttentionKind::InternalWork, "internal_work:1"),
        ];
        let (first, next, matching) = page(all.clone(), None, None, 2).unwrap();
        assert_eq!(matching, 5);
        assert_eq!(ids(&first), ["decision:a", "decision:b"]);
        let (second, next, _) = page(all.clone(), None, next.as_deref(), 2).unwrap();
        assert_eq!(ids(&second), ["result:1", "internal_work:1"]);
        let (last, next, _) = page(all, None, next.as_deref(), 2).unwrap();
        assert_eq!(ids(&last), ["internal_work:2"]);
        assert_eq!(next, None);
    }

    #[test]
    fn a_cursor_survives_its_entry_being_cleared() {
        let all = vec![
            item(AttentionKind::Failed, "result:1"),
            item(AttentionKind::Failed, "result:2"),
            item(AttentionKind::Failed, "result:3"),
        ];
        let (_, next, _) = page(all.clone(), None, None, 2).unwrap();
        let remaining = vec![all[0].clone(), all[2].clone()];
        let (rest, next, matching) = page(remaining, None, next.as_deref(), 2).unwrap();
        assert_eq!((ids(&rest), next, matching), (vec!["result:3"], None, 2));
    }

    #[test]
    fn the_filter_decides_membership_and_count() {
        let all = vec![
            item(AttentionKind::Decision, "decision:a"),
            item(AttentionKind::Failed, "result:1"),
        ];
        let (items, next, matching) = page(all, Some(&AttentionKind::Failed), None, 25).unwrap();
        assert_eq!((ids(&items), next, matching), (vec!["result:1"], None, 1));
        assert_eq!(AttentionKind::parse("failed"), Some(AttentionKind::Failed));
        assert_eq!(AttentionKind::parse("bogus"), None);
    }

    #[test]
    fn malformed_cursors_are_refused() {
        for cursor in ["", "x", "8:abc", "9:abc", "0:", "0:a b", "a:b"] {
            assert_eq!(
                page(vec![], None, Some(cursor), 1).unwrap_err(),
                PageError::Cursor,
                "{cursor}"
            );
        }
    }

    #[test]
    fn owner_kind_extensions_have_a_valid_cursor_rank() {
        assert_eq!(decode("7:admission:abc"), Ok((7, "admission:abc")));
        // Older readers can continue past a newer owner's kind even when that
        // kind's last entry has disappeared between requests.
        let (items, next, matching) = page(
            vec![item(AttentionKind::WorkerOffline, "worker:abc")],
            None,
            Some("7:admission:abc"),
            1,
        )
        .unwrap();
        assert!(items.is_empty());
        assert_eq!(next, None);
        assert_eq!(matching, 1);
    }

    fn sources(items: Vec<AttentionItem>) -> overview::OverviewSources {
        overview::OverviewSources {
            attempts: Default::default(),
            ready: Default::default(),
            reviews: overview::Contribution {
                needs_you: Some(overview::total(0, false)),
                ..Default::default()
            },
            results: overview::Contribution {
                attention: items,
                ..Default::default()
            },
            settlements: Default::default(),
            operations: Default::default(),
            internal: Default::default(),
        }
    }

    #[test]
    fn non_decision_overview_items_are_reachable_beyond_the_first_page() {
        // Reproduce the reported ten Needs you entries with an empty decisions
        // inbox, then exercise more entries than the default View All page.
        for count in [10, DEFAULT_LIMIT + 10] {
            let overview = sources(
                (0..count)
                    .map(|n| item(AttentionKind::Failed, &format!("result:{n:03}")))
                    .collect(),
            )
            .overview([], false);
            assert_eq!(overview.counts.needs_you.value, Some(count as u32));
            let (decisions, _, matching) = page(
                overview.attention.clone(),
                Some(&AttentionKind::Decision),
                None,
                DEFAULT_LIMIT,
            )
            .unwrap();
            assert!(decisions.is_empty());
            assert_eq!(matching, 0);

            let mut cursor = None;
            let mut listed = vec![];
            loop {
                let (items, next, matching) = page(
                    overview.attention.clone(),
                    None,
                    cursor.as_deref(),
                    DEFAULT_LIMIT,
                )
                .unwrap();
                assert_eq!(matching, count);
                listed.extend(items);
                cursor = next;
                if cursor.is_none() {
                    break;
                }
            }
            assert_eq!(ids(&listed), ids(&overview.attention));
        }
    }

    #[test]
    fn clearing_updates_both_views_and_cannot_hide_an_unresolved_decision() {
        let mut sources = sources(vec![
            item(AttentionKind::Failed, "result:1"),
            item(AttentionKind::Failed, "result:2"),
        ]);
        sources.reviews.attention = vec![item(AttentionKind::Decision, "decision:1")];
        let cleared = ["result:1".into(), "decision:1".into()].into();
        sources.without_cleared(&cleared);
        sources.without_cleared(&cleared);
        let overview = sources.overview([], false);
        assert_eq!(overview.counts.needs_you.value, Some(2));
        let (items, _, matching) = page(overview.attention, None, None, DEFAULT_LIMIT).unwrap();
        assert_eq!(matching, 2);
        assert_eq!(ids(&items), ["decision:1", "result:2"]);
        assert!(!items[0].dismissible);
        assert!(items[1].dismissible);
        let (_, _, matching) =
            page(items, Some(&AttentionKind::Failed), None, DEFAULT_LIMIT).unwrap();
        assert_eq!(matching, 1);
    }
    #[test]
    fn revision_fence_accepts_replay_and_gone_but_refuses_change() {
        assert!(check_revision("12", Some("12")).is_ok());
        assert!(check_revision("12", None).is_ok());
        assert!(matches!(
            check_revision("12", Some("13")),
            Err(RevisionError::Changed)
        ));
        assert!(matches!(
            check_revision("", None),
            Err(RevisionError::Invalid)
        ));
    }
    #[test]
    fn revision_filter_keeps_counts_lists_and_gates_in_agreement() {
        let subject = Uuid::new_v4();
        let make = |id: &str, revision: &str| {
            let mut item = contract::overview::attention(
                AttentionKind::InternalWork,
                EntityRef {
                    kind: "task".into(),
                    id: subject,
                    revision: Some(Revision(revision.into()).into()),
                },
                None,
                contract::overview::AttentionDescription {
                    title: "work",
                    reason: "failed",
                    since: None,
                },
            );
            item.id = id.into();
            item
        };
        let mut data = Overview::hidden();
        data.attention = vec![make("internal_work:a", "1"), make("internal_work:b", "2")];
        data.counts.needs_you = contract::overview::total(2, false);
        let cleared = [
            ("internal_work:a".into(), "1".into()),
            ("internal_work:b".into(), "1".into()),
        ]
        .into();
        let original = data.clone();
        filter_dismissed(
            &mut data,
            &cleared,
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(data.attention.len(), 1);
        assert_eq!(data.attention[0].id, "internal_work:b");
        assert_eq!(data.counts.needs_you.value, Some(1));
        let mut gated = original;
        filter_dismissed(&mut gated, &cleared, &[subject].into(), &Default::default());
        assert_eq!(gated.attention.len(), 2);
        assert_eq!(gated.counts.needs_you.value, Some(2));
        assert!(gated.attention.iter().all(|item| !item.dismissible));
    }
}

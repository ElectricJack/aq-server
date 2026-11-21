//! Where the work behind a recorded result is retained, for a reviewer who
//! must read the exact diff (a read; nothing here approves, settles, moves a
//! ref or recreates work).
//!
//! A delivery attempt's result is its task branch in central aq-git: the
//! report names `refs/heads/aq/<task>` at a head, and `delivery_pushed_results`
//! records that pair once aq-git showed the ref at it. "The branch ref is the
//! record of the result; no retention handle" (migration 0156), so a result's
//! work is found by `report_id` here, never by guessing an artifact id from
//! the result id. The base is the attempt's frozen git grant. A result with
//! no such row has no retained diff of its own: it reports `missing`, or
//! names its worker-claimed source receipts, which this module does not
//! resolve or vouch for.
//!
//! aq-git serves refs and name-status only, never patch bytes, so this
//! reports the exact provenance and a digest binding it, plus the steps that
//! read the diff from the retained commit.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgExecutor, Row};
use uuid::Uuid;

/// What the database recorded for one report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recorded {
    /// A `delivery_pushed_results` row exists for the report.
    pub row: bool,
    pub task_ref: Option<String>,
    /// NULL in the row: the push found its lease stale.
    pub head: Option<String>,
    pub repository_id: Option<Uuid>,
    pub base: Option<String>,
}

/// The recorded push and the attempt's git grant for `report_id`.
pub async fn recorded(db: impl PgExecutor<'_>, report_id: Uuid) -> sqlx::Result<Recorded> {
    let row = sqlx::query(
        "SELECT p.report_id IS NOT NULL AS row,p.task_ref,p.head,g.repository_id,g.base \
         FROM profiled_result_reports r \
         LEFT JOIN delivery_pushed_results p ON p.report_id=r.report_id \
         LEFT JOIN delivery_git_grants g ON g.attempt_id=r.attempt_id WHERE r.report_id=$1",
    )
    .bind(report_id)
    .fetch_optional(db)
    .await?;
    Ok(row.map_or_else(Recorded::default, |row| Recorded {
        row: row.get("row"),
        task_ref: row.get("task_ref"),
        head: row.get("head"),
        repository_id: row.get("repository_id"),
        base: row.get("base"),
    }))
}

/// The report facts the description binds to.
pub struct Subject<'a> {
    pub report_id: Uuid,
    pub revision: &'a str,
    pub attempt_id: Uuid,
    /// The execution spec's source repository and resolved base.
    pub source_repository_id: Uuid,
    pub source_base: &'a str,
    pub source_receipt_ids: &'a [Uuid],
}

/// Digest of the provenance: the report revision, repository, base, head and
/// ref. It is not a digest of patch bytes; the reader recomputes the diff from
/// `base` and `head` and compares those two commits, not this value.
pub fn provenance_digest(
    revision: &str,
    repository: Uuid,
    base: &str,
    head: &str,
    task_ref: &str,
) -> String {
    let text = format!("aq-result-work-v1\n{revision}\n{repository}\n{base}\n{head}\n{task_ref}");
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// The `work` object of a result revision. `state` is one of `pushed_ref`
/// (exact head retained), `push_conflict` (the push lost its lease: no head),
/// `source_receipts_only` (claimed receipts, nothing resolved here) or
/// `missing` (no retained work found; this reports that and recreates nothing).
pub fn describe(subject: &Subject<'_>, recorded: &Recorded) -> Value {
    let id = |v: &Uuid| v.to_string();
    let base = recorded.base.as_deref().unwrap_or(subject.source_base);
    let repository = recorded
        .repository_id
        .unwrap_or(subject.source_repository_id);
    let evidence = |state: &str, reason: &str| {
        json!({
            "state": state, "reason": reason,
            "report_id": id(&subject.report_id), "attempt_id": id(&subject.attempt_id),
            "revision": subject.revision,
            "source_receipt_ids": subject.source_receipt_ids,
            "patch": {"served": false},
        })
    };
    match (&recorded.task_ref, &recorded.head, recorded.row) {
        (Some(task_ref), Some(head), true) => {
            let digest = provenance_digest(subject.revision, repository, base, head, task_ref);
            json!({
                "state": "pushed_ref",
                "report_id": id(&subject.report_id), "attempt_id": id(&subject.attempt_id),
                "revision": subject.revision,
                "provenance": {
                    "repository_id": id(&repository), "base": base, "head": head,
                    "ref": task_ref, "digest": digest,
                    "digest_covers": "revision, repository, base, head and ref; not patch bytes",
                },
                "patch": {
                    "served": false,
                    "reason": "central aq-git serves refs and name-status, not patch bytes",
                    "retrieve": [
                        format!("git fetch <worker origin>/v1/git/{repository} {task_ref} (with the AQ-Attempt header of an attempt holding a live git grant for this repository)"),
                        format!("git rev-parse FETCH_HEAD must print {head}; refuse any other commit"),
                        format!("git diff --binary {base} {head}"),
                    ],
                    "note": "The ref is the record of the result; it may have moved since. The recorded head is exact, and aq-git 0.2.0 does not prune objects.",
                },
            })
        }
        (_, _, true) => evidence(
            "push_conflict",
            "the push lost its lease, so no head was retained for this report",
        ),
        _ if !subject.source_receipt_ids.is_empty() => evidence(
            "source_receipts_only",
            "the worker named source receipts; they are listed, not resolved or verified here",
        ),
        _ => evidence(
            "missing",
            "no retained task ref or source receipt is recorded for this report; \
             nothing was recreated and the work is not approved by this answer",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const HEAD: &str = "1111111111111111111111111111111111111111";
    const BASE: &str = "2222222222222222222222222222222222222222";
    fn subject(receipts: &[Uuid]) -> Subject<'_> {
        Subject {
            report_id: Uuid::from_u128(1),
            revision: "ab",
            attempt_id: Uuid::from_u128(2),
            source_repository_id: Uuid::from_u128(3),
            source_base: BASE,
            source_receipt_ids: receipts,
        }
    }
    fn pushed() -> Recorded {
        Recorded {
            row: true,
            task_ref: Some("refs/heads/aq/t".into()),
            head: Some(HEAD.into()),
            repository_id: Some(Uuid::from_u128(4)),
            base: Some("3333333333333333333333333333333333333333".into()),
        }
    }
    #[test]
    fn pushed_ref_names_exact_head_base_and_a_binding_digest() {
        let v = describe(&subject(&[]), &pushed());
        assert_eq!(v["state"], "pushed_ref");
        assert_eq!(v["provenance"]["head"], HEAD);
        // The frozen grant's base and repository win over the spec's.
        assert_eq!(
            v["provenance"]["repository_id"],
            Uuid::from_u128(4).to_string()
        );
        assert_eq!(
            v["provenance"]["base"],
            "3333333333333333333333333333333333333333"
        );
        assert_eq!(
            v["provenance"]["digest"],
            provenance_digest(
                "ab",
                Uuid::from_u128(4),
                "3333333333333333333333333333333333333333",
                HEAD,
                "refs/heads/aq/t"
            )
        );
        assert_eq!(v["patch"]["served"], false);
        assert!(v["patch"]["retrieve"][1].as_str().unwrap().contains(HEAD));
    }
    #[test]
    fn digest_changes_with_each_bound_field() {
        let r = Uuid::from_u128(4);
        let d = |rev, repo, base, head, rf| provenance_digest(rev, repo, base, head, rf);
        let one = d("ab", r, BASE, HEAD, "refs/heads/aq/t");
        for other in [
            d("ac", r, BASE, HEAD, "refs/heads/aq/t"),
            d("ab", Uuid::from_u128(5), BASE, HEAD, "refs/heads/aq/t"),
            d("ab", r, HEAD, HEAD, "refs/heads/aq/t"),
            d("ab", r, BASE, BASE, "refs/heads/aq/t"),
            d("ab", r, BASE, HEAD, "refs/heads/aq/u"),
        ] {
            assert_ne!(one, other);
        }
        assert_eq!(one.len(), 64);
    }
    #[test]
    fn a_lost_lease_missing_row_and_receipts_are_reported_without_a_head() {
        let lost = Recorded {
            row: true,
            task_ref: Some("refs/heads/aq/t".into()),
            ..Recorded::default()
        };
        assert_eq!(describe(&subject(&[]), &lost)["state"], "push_conflict");
        let receipt = [Uuid::from_u128(9)];
        let v = describe(&subject(&receipt), &Recorded::default());
        assert_eq!(v["state"], "source_receipts_only");
        assert_eq!(v["source_receipt_ids"][0], receipt[0].to_string());
        let v = describe(&subject(&[]), &Recorded::default());
        assert_eq!(v["state"], "missing");
        assert!(v.get("provenance").is_none());
        assert_eq!(v["patch"]["served"], false);
    }
}

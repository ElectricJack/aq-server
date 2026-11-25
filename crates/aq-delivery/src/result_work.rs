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
//! This reports the exact provenance and a digest binding it. The patch bytes
//! come from aq-git's `diff.patch` query ([`crate::diff_patch`]) where the
//! deployment offers it; otherwise the steps that read the diff from the
//! retained commit are listed.
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
    /// The repository binding generation of the attempt's launch, which aq-git
    /// checks every query against.
    pub binding_generation: Option<u64>,
}

/// The recorded push and the attempt's git grant for `report_id`.
pub async fn recorded(db: impl PgExecutor<'_>, report_id: Uuid) -> sqlx::Result<Recorded> {
    let row = sqlx::query(
        "SELECT p.report_id IS NOT NULL AS row,p.task_ref,p.head,g.repository_id,g.base,\
         (l.execution_spec#>>'{source,repository_binding_generation}')::bigint AS generation \
         FROM profiled_result_reports r \
         LEFT JOIN delivery_pushed_results p ON p.report_id=r.report_id \
         LEFT JOIN delivery_git_grants g ON g.attempt_id=r.attempt_id \
         LEFT JOIN profiled_launches l ON l.attempt_id=r.attempt_id WHERE r.report_id=$1",
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
        binding_generation: row
            .get::<Option<i64>, _>("generation")
            .and_then(|g| u64::try_from(g).ok()),
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

/// The task ref and commit range named by a retained result.
pub struct RetainedRef<'a> {
    pub base: &'a str,
    pub head: &'a str,
    pub task_ref: &'a str,
}

/// Digest of the provenance: the report revision, repository, base, head and
/// ref. It is not a digest of patch bytes; the reader recomputes the diff from
/// `base` and `head` and compares those two commits, not this value.
pub fn provenance_digest(revision: &str, repository: Uuid, reference: RetainedRef<'_>) -> String {
    let RetainedRef {
        base,
        head,
        task_ref,
    } = reference;
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
            let digest = provenance_digest(
                subject.revision,
                repository,
                RetainedRef {
                    base,
                    head,
                    task_ref,
                },
            );
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
                    "reason": "show returns provenance only; the exact bytes are read with result.review.patch",
                    "route": "/v1/agent/results/patch",
                    "requires": "aq-git capability diff.patch with limits.diff_patch_bytes",
                    "retrieve": [
                        format!("git fetch <worker origin>/v1/git/{repository} {task_ref} (with the AQ-Attempt header of an attempt holding a live git grant for this repository)"),
                        format!("git rev-parse FETCH_HEAD must print {head}; refuse any other commit"),
                        format!("git diff --binary {base} {head}"),
                    ],
                    "note": "The ref is the record of the result; it may have moved since. The recorded head is exact. retrieve is the fallback for an aq-git without diff.patch.",
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
            binding_generation: Some(1),
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
                RetainedRef {
                    base: "3333333333333333333333333333333333333333",
                    head: HEAD,
                    task_ref: "refs/heads/aq/t"
                }
            )
        );
        assert_eq!(v["patch"]["served"], false);
        assert!(v["patch"]["retrieve"][1].as_str().unwrap().contains(HEAD));
    }
    /// The reviewed result df3d2e44…@44a9f35e…: head 105c94ad… from
    /// `delivery_pushed_results`, base 957a001e… from its launch, with no
    /// legacy source receipts.
    #[test]
    fn the_reviewed_result_binds_its_verified_head_to_its_base_and_revision() {
        const REVISION: &str = "44a9f35e560162b48354d5374ec9c9ccc419a757ec82b5e0b52e18dbddbe8f89";
        let (head, base) = (
            "105c94ad4a258ab7b943f14bbd31db935ec9af5e",
            "957a001e392c4405b00ea1f11a1e621b19842447",
        );
        let repository: Uuid = "674b55a3-94ec-55b4-8338-df9c4fc8a0ed".parse().unwrap();
        let subject = Subject {
            report_id: "df3d2e44-fdcb-42c9-aaaf-dbd47a1f9aa1".parse().unwrap(),
            revision: REVISION,
            attempt_id: Uuid::from_u128(2),
            source_repository_id: repository,
            source_base: base,
            source_receipt_ids: &[],
        };
        let task_ref = "refs/heads/aq/f86d9749-4371-46ec-b302-815078c1718e";
        let row = Recorded {
            row: true,
            task_ref: Some(task_ref.into()),
            head: Some(head.into()),
            repository_id: Some(repository),
            base: Some(base.into()),
            binding_generation: Some(1),
        };
        let v = describe(&subject, &row);
        assert_eq!(v["state"], "pushed_ref");
        assert_eq!(v["provenance"]["head"], head);
        assert_eq!(v["provenance"]["base"], base);
        assert_eq!(
            v["provenance"]["digest"],
            provenance_digest(
                REVISION,
                repository,
                RetainedRef {
                    base,
                    head,
                    task_ref
                }
            )
        );
        // Empty legacy receipts do not hide the retained ref; a changed
        // revision binds a different digest.
        let changed = Subject {
            revision: &REVISION.replace('4', "5"),
            ..subject
        };
        assert_ne!(
            describe(&changed, &row)["provenance"]["digest"],
            v["provenance"]["digest"]
        );
        // Without the row the same result reports its source as missing.
        assert_eq!(describe(&subject, &Recorded::default())["state"], "missing");
    }
    #[test]
    fn digest_changes_with_each_bound_field() {
        let r = Uuid::from_u128(4);
        let d = |rev, repo, base, head, rf| {
            provenance_digest(
                rev,
                repo,
                RetainedRef {
                    base,
                    head,
                    task_ref: rf,
                },
            )
        };
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

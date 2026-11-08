//! Fair multi-project scheduling (worker spec §8.2): configured priority plus
//! aging, with a hard starvation bound. Pure ordering; the durable last-service
//! record stays with its store owner in aq-server (`producer::fairness`).
use std::collections::BTreeMap;
use uuid::Uuid;

/// A project waiting longer than this is served before any priority order.
pub const STARVATION_MS: u64 = 10 * 60_000;
/// Aging gains one priority point per this much waiting.
pub const AGING_MS: u64 = 1_000;

/// One enabled project's scheduling inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub project_id: Uuid,
    pub priority: i32,
}

/// Projects in the order this poll should try them. A never-served project is
/// treated as waiting exactly `STARVATION_MS`, so new projects are not starved
/// and do not jump ahead of older starving ones.
pub fn order(projects: &[Candidate], served: &BTreeMap<Uuid, u64>, now: u64) -> Vec<Candidate> {
    let waited = |p: &Candidate| {
        served
            .get(&p.project_id)
            .map_or(STARVATION_MS, |last| now.saturating_sub(*last))
    };
    let mut ordered = projects.to_vec();
    ordered.sort_by_key(|p| {
        let waited = waited(p);
        let starving = waited > STARVATION_MS;
        let effective =
            i64::from(p.priority) + i64::try_from(waited / AGING_MS).unwrap_or(i64::MAX);
        let rank = if starving {
            i64::try_from(waited).unwrap_or(i64::MAX)
        } else {
            effective
        };
        (
            !starving,
            std::cmp::Reverse(rank),
            std::cmp::Reverse(waited),
            p.project_id,
        )
    });
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_projects_share_capacity_fairly() {
        let high = Candidate {
            project_id: Uuid::from_u128(2),
            priority: 100,
        };
        let low = Candidate {
            project_id: Uuid::from_u128(1),
            priority: 0,
        };
        let projects = [low, high];
        let mut served = BTreeMap::new();
        let mut counts = BTreeMap::<Uuid, u32>::new();
        let mut now = 1_000_000_u64;
        let mut longest_wait = BTreeMap::<Uuid, u64>::new();
        // One slot per 10s poll, both projects always ready.
        for _ in 0..600 {
            let first = order(&projects, &served, now)[0];
            if let Some(last) = served.get(&first.project_id) {
                let wait = now - last;
                let entry = longest_wait.entry(first.project_id).or_default();
                *entry = (*entry).max(wait);
            }
            served.insert(first.project_id, now);
            *counts.entry(first.project_id).or_default() += 1;
            now += 10_000;
        }
        let (h, l) = (counts[&high.project_id], counts[&low.project_id]);
        // Priority is honoured: the higher-priority project gets more capacity.
        assert!(h > l, "high={h} low={l}");
        // Aging prevents starvation: the low project still gets a bounded share
        // and never waits past the starvation bound plus one poll.
        assert!(l * 20 >= h, "low share starved: high={h} low={l}");
        assert!(longest_wait[&low.project_id] <= STARVATION_MS + 10_000);
        // Equal priority alternates exactly.
        let peer = Candidate {
            project_id: Uuid::from_u128(3),
            priority: 100,
        };
        let mut served = BTreeMap::new();
        let mut picks = Vec::new();
        for step in 0..6_u64 {
            let first = order(&[high, peer], &served, 1_000 + step)[0];
            served.insert(first.project_id, 1_000 + step);
            picks.push(first.project_id);
        }
        assert_eq!(
            picks,
            [2, 3, 2, 3, 2, 3].map(Uuid::from_u128).to_vec(),
            "equal priority must alternate"
        );
        // Waiting gains priority: a long-unserved low project outranks a just-served high one.
        let served = BTreeMap::from([(high.project_id, 199_000), (low.project_id, 1)]);
        assert_eq!(order(&projects, &served, 200_000)[0], low);
        // A never-served project is not starved by served ones.
        let served = BTreeMap::from([(high.project_id, 100_000 - 1)]);
        assert_eq!(order(&projects, &served, 100_000)[0], low);
    }
}

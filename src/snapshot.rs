//! Change detection. A `Snapshot` captures the *meaningful* values of every
//! section (not the rendered ANSI) so we can ring the bell exactly when
//! something a maintainer cares about changes: a new/removed/merged PR, a queue
//! position or state change, a status / mergeability / fail-count change, or a
//! title change.

use crate::merged::MergedRow;
use crate::prs::PrRow;
use crate::queue::QueueRow;
use crate::status::Status;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Snapshot {
    pub merged: Vec<MergedSnap>,
    pub queue: Vec<QueueSnap>,
    pub prs: Vec<PrSnap>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergedSnap {
    pub number: i64,
    pub merged_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueSnap {
    pub position: i64,
    pub number: i64,
    pub author: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrSnap {
    pub number: i64,
    pub is_draft: bool,
    pub title: String,
    pub status: Option<Status>,
    pub merge_state: Option<String>,
    pub queue: Option<(i64, String)>,
    pub fail: usize,
}

impl Snapshot {
    /// Build a snapshot from the (optional) rows of each section. Disabled
    /// sections contribute an empty vec, so toggling `--only` never registers
    /// as a change on its own.
    pub fn build(
        merged: Option<&[MergedRow]>,
        queue: Option<&[QueueRow]>,
        prs: Option<&[PrRow]>,
    ) -> Snapshot {
        Snapshot {
            merged: merged
                .unwrap_or(&[])
                .iter()
                .map(|r| MergedSnap {
                    number: r.number,
                    merged_at: r.merged_at.clone(),
                })
                .collect(),
            queue: queue
                .unwrap_or(&[])
                .iter()
                .map(|r| QueueSnap {
                    position: r.position,
                    number: r.number,
                    author: r.author.clone(),
                    title: r.title.clone(),
                })
                .collect(),
            prs: prs
                .unwrap_or(&[])
                .iter()
                .map(|r| PrSnap {
                    number: r.number,
                    is_draft: r.is_draft,
                    title: r.title.clone(),
                    status: r.status,
                    merge_state: r.merge_state.clone(),
                    queue: r.queue.clone(),
                    fail: r.fail,
                })
                .collect(),
        }
    }

    /// PR numbers present in `self.merged` but not in `prev.merged` — i.e. PRs
    /// that just merged. Used for the desktop notification body.
    pub fn newly_merged(&self, prev: &Snapshot) -> Vec<i64> {
        self.merged
            .iter()
            .filter(|m| !prev.merged.iter().any(|p| p.number == m.number))
            .map(|m| m.number)
            .collect()
    }
}

/// Whether this refresh should ring the bell. The first refresh
/// (`prev == None`) never rings; afterwards we ring iff the snapshot differs.
/// Because the caller stores `now` as the next `prev`, this rings *exactly
/// once* per changed refresh.
pub fn should_ring(prev: Option<&Snapshot>, now: &Snapshot) -> bool {
    matches!(prev, Some(p) if p != now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr_row(number: i64, status: Option<Status>, fail: usize, title: &str) -> PrRow {
        PrRow {
            number,
            is_draft: false,
            title: title.to_string(),
            status,
            merge_state: Some("CLEAN".to_string()),
            queue: None,
            fail,
            url: format!("https://x/{number}"),
        }
    }

    fn merged_row(number: i64, at: &str) -> MergedRow {
        MergedRow {
            number,
            title: format!("PR {number}"),
            url: format!("https://x/{number}"),
            base: "main".to_string(),
            merged_at: Some(at.to_string()),
        }
    }

    #[test]
    fn identical_rows_are_unchanged() {
        let a = vec![pr_row(1, Some(Status::Pass), 0, "t")];
        let s1 = Snapshot::build(None, None, Some(&a));
        let s2 = Snapshot::build(None, None, Some(&a));
        assert_eq!(s1, s2);
    }

    #[test]
    fn status_change_is_detected() {
        let before = Snapshot::build(None, None, Some(&[pr_row(1, Some(Status::Pending), 0, "t")]));
        let after = Snapshot::build(None, None, Some(&[pr_row(1, Some(Status::Pass), 0, "t")]));
        assert_ne!(before, after);
    }

    #[test]
    fn fail_count_and_title_changes_are_detected() {
        let base = pr_row(1, Some(Status::Fail), 1, "t");
        let s = Snapshot::build(None, None, Some(&[base.clone()]));
        let more_fails = Snapshot::build(None, None, Some(&[pr_row(1, Some(Status::Fail), 2, "t")]));
        let retitled = Snapshot::build(None, None, Some(&[pr_row(1, Some(Status::Fail), 1, "t2")]));
        assert_ne!(s, more_fails);
        assert_ne!(s, retitled);
    }

    #[test]
    fn a_pr_merging_is_detected_and_reported() {
        let before = Snapshot::build(None, None, Some(&[pr_row(7, Some(Status::Pass), 0, "t")]));
        // PR #7 dropped out of "my PRs" and appeared in "recently merged".
        let after = Snapshot::build(Some(&[merged_row(7, "2026-06-19T00:00:00Z")]), None, None);
        assert_ne!(before, after);
        assert_eq!(after.newly_merged(&before), vec![7]);
    }

    #[test]
    fn disabled_sections_do_not_register_as_change() {
        let prs = vec![pr_row(1, Some(Status::Pass), 0, "t")];
        // Queue enabled-but-empty vs disabled both yield an empty queue vec.
        let enabled_empty = Snapshot::build(None, Some(&[]), Some(&prs));
        let disabled = Snapshot::build(None, None, Some(&prs));
        assert_eq!(enabled_empty, disabled);
    }

    #[test]
    fn rings_exactly_once_per_changed_refresh() {
        let a = Snapshot::build(None, None, Some(&[pr_row(1, Some(Status::Pending), 0, "t")]));
        let b = Snapshot::build(None, None, Some(&[pr_row(1, Some(Status::Pass), 0, "t")]));
        // Sequence of fetched snapshots: A, A, B, B, A.
        let seq = [a.clone(), a.clone(), b.clone(), b.clone(), a.clone()];
        let mut prev: Option<Snapshot> = None;
        let mut rings = Vec::new();
        for s in &seq {
            rings.push(should_ring(prev.as_ref(), s));
            prev = Some(s.clone());
        }
        // First never rings; rings only on the A->B and B->A transitions.
        assert_eq!(rings, vec![false, false, true, false, true]);
    }
}

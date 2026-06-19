//! Change detection for the bell. We ring on exactly two events: a PR of mine
//! merging, and an open PR's CI/merge status changing between refreshes. Both
//! are keyed by PR number, so re-sorting (e.g. by update time) never rings.

use crate::merged::MergedRow;
use crate::prs::PrRow;
use crate::status::Status;
use std::collections::{HashMap, HashSet};

/// The bell-relevant state of one refresh.
#[derive(Debug, Default, Clone)]
pub struct Tracker {
    open_status: HashMap<i64, Option<Status>>,
    merged: HashSet<i64>,
}

/// What changed between the previous refresh and the current one.
#[derive(Debug, Default, Clone)]
pub struct Changes {
    /// Open PRs whose status changed (highlighted in the Open PRs table).
    pub status_changed: HashSet<i64>,
    /// PRs that newly appeared in the merged list (highlighted there).
    pub newly_merged: HashSet<i64>,
}

impl Changes {
    /// Whether anything bell-worthy happened.
    pub fn any(&self) -> bool {
        !self.status_changed.is_empty() || !self.newly_merged.is_empty()
    }
}

impl Tracker {
    pub fn build(open: Option<&[PrRow]>, merged: Option<&[MergedRow]>) -> Tracker {
        Tracker {
            open_status: open
                .unwrap_or(&[])
                .iter()
                .map(|r| (r.number, r.status))
                .collect(),
            merged: merged.unwrap_or(&[]).iter().map(|r| r.number).collect(),
        }
    }

    /// Changes from `prev` (previous refresh) to `self` (current refresh). A PR
    /// must exist in both refreshes to count as a status change; a PR appearing
    /// in `merged` for the first time counts as newly merged.
    pub fn diff(&self, prev: &Tracker) -> Changes {
        let status_changed = self
            .open_status
            .iter()
            .filter(|(num, status)| matches!(prev.open_status.get(num), Some(p) if p != *status))
            .map(|(num, _)| *num)
            .collect();
        let newly_merged = self
            .merged
            .iter()
            .filter(|num| !prev.merged.contains(num))
            .copied()
            .collect();
        Changes {
            status_changed,
            newly_merged,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: i64, status: Option<Status>) -> PrRow {
        PrRow {
            number,
            is_draft: false,
            title: format!("PR {number}"),
            status,
            merge_state: Some("CLEAN".to_string()),
            queue: None,
            fail: 0,
            url: format!("https://x/{number}"),
            updated_at: Some("2026-06-19T00:00:00Z".to_string()),
        }
    }

    fn merged(number: i64) -> MergedRow {
        MergedRow {
            number,
            title: format!("PR {number}"),
            url: format!("https://x/{number}"),
            base: "main".to_string(),
            merged_at: Some("2026-06-19T00:00:00Z".to_string()),
            updated_at: Some("2026-06-19T00:00:00Z".to_string()),
        }
    }

    #[test]
    fn status_change_rings_and_is_pinpointed() {
        let before = Tracker::build(
            Some(&[pr(1, Some(Status::Pending)), pr(2, Some(Status::Pass))]),
            None,
        );
        let after = Tracker::build(
            Some(&[pr(1, Some(Status::Pass)), pr(2, Some(Status::Pass))]),
            None,
        );
        let c = after.diff(&before);
        assert!(c.any());
        assert_eq!(c.status_changed, HashSet::from([1]));
        assert!(c.newly_merged.is_empty());
    }

    #[test]
    fn merging_rings() {
        let before = Tracker::build(Some(&[pr(7, Some(Status::Pass))]), Some(&[]));
        // #7 dropped out of open PRs and showed up in merged.
        let after = Tracker::build(Some(&[]), Some(&[merged(7)]));
        let c = after.diff(&before);
        assert!(c.any());
        assert_eq!(c.newly_merged, HashSet::from([7]));
    }

    #[test]
    fn reordering_and_new_prs_do_not_ring() {
        let before = Tracker::build(
            Some(&[pr(1, Some(Status::Pass)), pr(2, Some(Status::Pass))]),
            None,
        );
        // Same two PRs, swapped order, plus a brand-new PR #3.
        let after = Tracker::build(
            Some(&[
                pr(2, Some(Status::Pass)),
                pr(1, Some(Status::Pass)),
                pr(3, Some(Status::Fail)),
            ]),
            None,
        );
        assert!(!after.diff(&before).any());
    }

    #[test]
    fn first_refresh_is_silent() {
        let cur = Tracker::build(Some(&[pr(1, Some(Status::Fail))]), Some(&[merged(2)]));
        // No previous tracker -> default Changes -> nothing rings.
        assert!(!Changes::default().any());
        let _ = cur;
    }
}

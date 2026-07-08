//! Row navigation for the watch dashboard: the ordered list of open targets for
//! a view (mirroring the render order) and the selection-cursor movement. Watch
//! mode only — `--once`/piped output never has a selection.

use crate::Sections;
use crate::cli::View;
use crate::term::Wait;

/// The open URL of every navigable row in `view`, in the exact top-to-bottom
/// order the dashboard renders them, so a selection index lines up with the
/// rendered rows. Rows without a URL (an "upcoming" shipments row with no
/// commits) are skipped, matching the caret placement in `render`.
pub(crate) fn targets(view: View, s: &Sections) -> Vec<&str> {
    let mut urls: Vec<&str> = Vec::new();
    match view {
        View::Mine => {
            if let Some(rows) = &s.prs {
                urls.extend(rows.iter().map(|r| r.url.as_str()));
            }
            if let Some(rows) = &s.queue {
                urls.extend(rows.iter().map(|r| r.url.as_str()));
            }
            if let Some(rows) = &s.merged {
                urls.extend(rows.iter().map(|r| r.url.as_str()));
            }
            if let Some(stats) = &s.commits
                && stats.available
            {
                urls.extend(stats.upcoming.iter().map(|b| b.url.as_str()));
                urls.extend(stats.releases.iter().map(|r| r.bucket.url.as_str()));
            }
        }
        View::Reviews => {
            if let Some(rows) = &s.reviews {
                urls.extend(rows.iter().map(|r| r.url.as_str()));
            }
            if let Some(rows) = &s.reviewed_merged {
                urls.extend(rows.iter().map(|r| r.url.as_str()));
            }
        }
    }
    urls
}

/// The new selection after a movement key against a `len`-row list (`half` is
/// the half-page step). From no selection, any move enters at the top except
/// `Bottom`, which enters at the last row. Non-movement actions leave the
/// selection unchanged.
pub(crate) fn moved(action: Wait, sel: Option<usize>, len: usize, half: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let last = len - 1;
    let step = half.max(1);
    let new = match action {
        Wait::Top => 0,
        Wait::Bottom => last,
        Wait::Up => sel.map_or(0, |i| i.saturating_sub(1)),
        Wait::Down => sel.map_or(0, |i| (i + 1).min(last)),
        Wait::HalfUp => sel.map_or(0, |i| i.saturating_sub(step)),
        Wait::HalfDown => sel.map_or(0, |i| (i + step).min(last)),
        _ => return sel,
    };
    Some(new)
}

/// Clamp a selection to a (possibly shrunk) list after a refresh: drop it when
/// the list is empty, else pin it to the last row if it fell off the end.
pub(crate) fn clamp(sel: Option<usize>, len: usize) -> Option<usize> {
    match sel {
        Some(i) if len > 0 => Some(i.min(len - 1)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commits::{Bucket, CommitStats, Count, Release};
    use crate::merged::MergedRow;
    use crate::prs::PrRow;
    use crate::queue::QueueRow;
    use crate::reviews::{ReviewRow, ReviewedMergedRow};
    use crate::status::ReviewState;

    fn pr(n: i64) -> PrRow {
        PrRow {
            number: n,
            is_draft: false,
            title: format!("pr {n}"),
            status: None,
            merge_state: None,
            queue: None,
            fail: 0,
            url: format!("https://pr/{n}"),
            updated_at: None,
        }
    }

    fn queued(n: i64) -> QueueRow {
        QueueRow {
            position: n,
            number: n,
            author: "me".into(),
            title: format!("q {n}"),
            url: format!("https://q/{n}"),
            mine: true,
            enqueued_at: None,
            build_started_at: None,
        }
    }

    fn merged(n: i64) -> MergedRow {
        MergedRow {
            number: n,
            title: format!("m {n}"),
            url: format!("https://m/{n}"),
            release: None,
            merged_at: None,
        }
    }

    fn bucket(url: &str) -> Bucket {
        Bucket {
            count: Count {
                mine: 1,
                capped: false,
            },
            url: url.into(),
        }
    }

    fn empty() -> Sections {
        Sections {
            merged: None,
            queue: None,
            queue_next_eta: None,
            prs: None,
            commits: None,
            reviews: None,
            reviewed_merged: None,
        }
    }

    #[test]
    fn mine_targets_follow_render_order() {
        let mut s = empty();
        s.prs = Some(vec![pr(1), pr(2)]);
        s.queue = Some(vec![queued(3)]);
        s.merged = Some(vec![merged(4)]);
        s.commits = Some(CommitStats {
            available: true,
            upcoming: Some(bucket("https://up")),
            releases: vec![Release {
                tag: "v1".into(),
                bucket: bucket("https://rel/v1"),
                published_at: None,
            }],
        });
        assert_eq!(
            targets(View::Mine, &s),
            vec![
                "https://pr/1",
                "https://pr/2",
                "https://q/3",
                "https://m/4",
                "https://up",
                "https://rel/v1",
            ]
        );
    }

    #[test]
    fn unavailable_or_urlless_shipments_are_skipped() {
        let mut s = empty();
        s.prs = Some(vec![pr(1)]);
        // No "upcoming" commits and stats marked unavailable -> no shipment targets.
        s.commits = Some(CommitStats {
            available: false,
            upcoming: None,
            releases: vec![],
        });
        assert_eq!(targets(View::Mine, &s), vec!["https://pr/1"]);
    }

    #[test]
    fn reviews_targets_follow_render_order() {
        let mut s = empty();
        s.reviews = Some(vec![ReviewRow {
            number: 1,
            is_draft: false,
            title: "r".into(),
            author: "a".into(),
            url: "https://rev/1".into(),
            state: ReviewState::Awaiting,
            updated_at: None,
        }]);
        s.reviewed_merged = Some(vec![ReviewedMergedRow {
            number: 2,
            title: "rm".into(),
            author: "a".into(),
            url: "https://revm/2".into(),
            merged_at: None,
        }]);
        assert_eq!(
            targets(View::Reviews, &s),
            vec!["https://rev/1", "https://revm/2"]
        );
    }

    #[test]
    fn movement_enters_and_clamps() {
        // From no selection, a down-ish move enters at the top; Bottom at the end.
        assert_eq!(moved(Wait::Down, None, 5, 2), Some(0));
        assert_eq!(moved(Wait::Up, None, 5, 2), Some(0));
        assert_eq!(moved(Wait::Bottom, None, 5, 2), Some(4));
        // Stepping is clamped to the ends.
        assert_eq!(moved(Wait::Down, Some(4), 5, 2), Some(4));
        assert_eq!(moved(Wait::Up, Some(0), 5, 2), Some(0));
        assert_eq!(moved(Wait::HalfDown, Some(0), 5, 2), Some(2));
        assert_eq!(moved(Wait::HalfUp, Some(4), 5, 2), Some(2));
        assert_eq!(moved(Wait::Top, Some(3), 5, 2), Some(0));
        // An empty list has nothing to select; non-movement keys are inert.
        assert_eq!(moved(Wait::Down, Some(0), 0, 2), None);
        assert_eq!(moved(Wait::Open, Some(1), 5, 2), Some(1));
    }

    #[test]
    fn clamp_pins_or_drops() {
        assert_eq!(clamp(Some(9), 5), Some(4));
        assert_eq!(clamp(Some(2), 5), Some(2));
        assert_eq!(clamp(Some(0), 0), None);
        assert_eq!(clamp(None, 5), None);
    }
}

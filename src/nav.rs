//! Row navigation for the watch dashboard: the ordered list of open targets for
//! a view (mirroring the render order) and the selection-cursor movement. Watch
//! mode only — `--once`/piped output never has a selection.

use crate::Sections;
use crate::cli::View;
use crate::commits::CommitStats;
use crate::merged::MergedRow;
use crate::prs::PrRow;
use crate::queue::QueueRow;
use crate::reviews::{ReviewRow, ReviewedMergedRow};
use crate::term::Wait;

/// A row the search can match (its `haystack`) and the cursor can open (`url`).
trait Searchable {
    /// PR number, title, and (where present) author or release tag, joined for a
    /// single case-insensitive substring test.
    fn haystack(&self) -> String;
    fn url(&self) -> &str;
}

impl Searchable for PrRow {
    fn haystack(&self) -> String {
        format!("#{} {}", self.number, self.title)
    }
    fn url(&self) -> &str {
        &self.url
    }
}
impl Searchable for QueueRow {
    fn haystack(&self) -> String {
        format!("#{} {} {}", self.number, self.title, self.author)
    }
    fn url(&self) -> &str {
        &self.url
    }
}
impl Searchable for MergedRow {
    fn haystack(&self) -> String {
        let tag = self.release.as_ref().map_or("", |x| x.tag.as_str());
        format!("#{} {} {}", self.number, self.title, tag)
    }
    fn url(&self) -> &str {
        &self.url
    }
}
impl Searchable for ReviewRow {
    fn haystack(&self) -> String {
        format!("#{} {} {}", self.number, self.title, self.author)
    }
    fn url(&self) -> &str {
        &self.url
    }
}
impl Searchable for ReviewedMergedRow {
    fn haystack(&self) -> String {
        format!("#{} {} {}", self.number, self.title, self.author)
    }
    fn url(&self) -> &str {
        &self.url
    }
}

/// Whether `hay` matches the (already-lowercased) query; an empty query matches
/// everything.
fn hit(hay: &str, query_lower: &str) -> bool {
    query_lower.is_empty() || hay.to_lowercase().contains(query_lower)
}

/// Push, in order, the URLs of the rows in `rows` (if present) that match the
/// already-lowercased `query`.
fn push_matches<'a, T: Searchable>(urls: &mut Vec<&'a str>, rows: Option<&'a [T]>, query: &str) {
    if let Some(rows) = rows {
        urls.extend(
            rows.iter()
                .filter(|r| hit(&r.haystack(), query))
                .map(Searchable::url),
        );
    }
}

/// The open URL of every navigable row in `view` that matches `query`, in the
/// exact top-to-bottom order the dashboard renders them, so a selection index
/// lines up with the rendered (and identically filtered) rows. Rows without a
/// URL (an "upcoming" shipments row with no commits) are skipped. An empty
/// `query` yields every row.
pub(crate) fn targets<'a>(view: View, s: &'a Sections, query: &str) -> Vec<&'a str> {
    let q = query.to_lowercase();
    let mut urls: Vec<&str> = Vec::new();
    match view {
        View::Mine => {
            push_matches(&mut urls, s.prs.as_deref(), &q);
            push_matches(&mut urls, s.queue.as_deref(), &q);
            push_matches(&mut urls, s.merged.as_deref(), &q);
            if let Some(stats) = &s.commits
                && stats.available
            {
                if let Some(b) = &stats.upcoming
                    && hit("upcoming", &q)
                {
                    urls.push(b.url.as_str());
                }
                urls.extend(
                    stats
                        .releases
                        .iter()
                        .filter(|r| hit(&r.tag, &q))
                        .map(|r| r.bucket.url.as_str()),
                );
            }
        }
        View::Reviews => {
            push_matches(&mut urls, s.reviews.as_deref(), &q);
            push_matches(&mut urls, s.reviewed_merged.as_deref(), &q);
        }
    }
    urls
}

/// A copy of `s` keeping only the rows that match `query` (every section, both
/// views), for rendering the filtered dashboard. Uses the same per-row haystack
/// as `targets`, so the rendered rows and the navigable targets stay in lockstep.
pub(crate) fn filter(s: &Sections, query: &str) -> Sections {
    let q = query.to_lowercase();
    Sections {
        prs: s.prs.as_deref().map(|r| matching(r, &q)),
        queue: s.queue.as_deref().map(|r| matching(r, &q)),
        queue_next_eta: s.queue_next_eta,
        merged: s.merged.as_deref().map(|r| matching(r, &q)),
        commits: s.commits.as_ref().map(|c| filter_commits(c, &q)),
        reviews: s.reviews.as_deref().map(|r| matching(r, &q)),
        reviewed_merged: s.reviewed_merged.as_deref().map(|r| matching(r, &q)),
    }
}

/// Clone the rows whose haystack matches the already-lowercased `query`.
fn matching<T: Searchable + Clone>(rows: &[T], query: &str) -> Vec<T> {
    retain(rows, |x| hit(&x.haystack(), query))
}

/// Clone the elements of `rows` that satisfy `keep`.
fn retain<T: Clone>(rows: &[T], keep: impl Fn(&T) -> bool) -> Vec<T> {
    rows.iter().filter(|x| keep(x)).cloned().collect()
}

/// Filter the shipments: releases by tag, the "upcoming" bucket by the literal
/// "upcoming" (so it drops out of a tag search).
fn filter_commits(stats: &CommitStats, query_lower: &str) -> CommitStats {
    CommitStats {
        available: stats.available,
        upcoming: stats
            .upcoming
            .clone()
            .filter(|_| hit("upcoming", query_lower)),
        releases: retain(&stats.releases, |r| hit(&r.tag, query_lower)),
    }
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
            targets(View::Mine, &s, ""),
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
        assert_eq!(targets(View::Mine, &s, ""), vec!["https://pr/1"]);
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
            targets(View::Reviews, &s, ""),
            vec!["https://rev/1", "https://revm/2"]
        );
    }

    #[test]
    fn query_filters_targets_by_number_title_author_and_tag() {
        let mut s = empty();
        s.prs = Some(vec![pr(1), pr(2)]); // titles "pr 1" / "pr 2"
        s.queue = Some(vec![queued(3)]); // author "me"
        s.merged = Some(vec![merged(4)]);
        s.commits = Some(CommitStats {
            available: true,
            upcoming: Some(bucket("https://up")),
            releases: vec![Release {
                tag: "v1.5.0".into(),
                bucket: bucket("https://rel/v1"),
                published_at: None,
            }],
        });
        // Number substring hits the matching PR only.
        assert_eq!(targets(View::Mine, &s, "#2"), vec!["https://pr/2"]);
        // Author (case-insensitive) hits the queue row.
        assert_eq!(targets(View::Mine, &s, "ME"), vec!["https://q/3"]);
        // Release tag hits the release; "upcoming" hits the upcoming bucket.
        assert_eq!(targets(View::Mine, &s, "v1.5"), vec!["https://rel/v1"]);
        assert_eq!(targets(View::Mine, &s, "upcoming"), vec!["https://up"]);
        // No match -> empty.
        assert!(targets(View::Mine, &s, "zzz").is_empty());
    }

    #[test]
    fn filter_keeps_matching_rows_in_lockstep_with_targets() {
        let mut s = empty();
        s.prs = Some(vec![pr(1), pr(2)]);
        s.merged = Some(vec![merged(4)]);
        let f = filter(&s, "#2");
        assert_eq!(f.prs.as_ref().unwrap().len(), 1);
        assert_eq!(f.prs.as_ref().unwrap()[0].number, 2);
        assert!(f.merged.as_ref().unwrap().is_empty());
        // The filtered sections' targets equal the query-filtered targets.
        assert_eq!(targets(View::Mine, &f, ""), targets(View::Mine, &s, "#2"));
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

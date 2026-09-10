//! Row navigation for the watch dashboard: the ordered list of open targets for
//! a view (mirroring the render order) and the selection-cursor movement. Watch
//! mode only — `--once`/piped output never has a selection.

use crate::cli::View;
use crate::commits::CommitStats;
use crate::merged::MergedRow;
use crate::prs::PrRow;
use crate::queue::QueueRow;
use crate::reviews::{ReviewRow, ReviewedMergedRow};
use crate::{Sections, Visibility};

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

/// The URLs of the rows in `rows` (if present) matching the already-lowercased
/// `query` — one rendered section's worth of navigable targets.
fn group<'a, T: Searchable>(rows: Option<&'a [T]>, query: &str) -> Vec<&'a str> {
    rows.unwrap_or_default()
        .iter()
        .filter(|row| hit(&row.haystack(), query))
        .map(Searchable::url)
        .collect()
}

fn when_visible<T>(rows: Option<&[T]>, visible: bool) -> Option<&[T]> {
    rows.filter(|_| visible)
}

fn prs_group<'a>(rows: Option<&'a [PrRow]>, query: &str, queue_visible: bool) -> Vec<&'a str> {
    rows.map_or_else(Vec::new, |rows| {
        rows.iter()
            .filter(|row| !queue_visible || row.queue.is_none())
            .filter(|row| hit(&row.haystack(), query))
            .map(Searchable::url)
            .collect()
    })
}

/// The "My Shipments" section's targets: the "upcoming" compare log (when there
/// is one) then each release page, matched against the already-lowercased
/// `query`. Empty when the release lookup failed (`available` is false).
fn shipments<'a>(s: &'a Sections, query: &str) -> Vec<&'a str> {
    let mut urls = Vec::new();
    if let Some(stats) = &s.commits
        && stats.available
    {
        if let Some(b) = &stats.upcoming
            && hit("upcoming", query)
        {
            urls.push(b.url.as_str());
        }
        urls.extend(
            stats
                .releases
                .iter()
                .filter(|r| hit(&r.tag, query))
                .map(|r| r.bucket.url.as_str()),
        );
    }
    urls
}

/// The navigable targets of `view` matching `query`, grouped by rendered
/// section and in render order. `targets` is this flattened, so the two can't
/// drift; `section_at` uses the grouping to answer "every link in the section
/// the cursor is in".
fn groups<'a>(view: View, s: &'a Sections, query: &str, visible: Visibility) -> Vec<Vec<&'a str>> {
    let q = query.to_lowercase();
    match view {
        View::Mine => vec![
            prs_group(
                when_visible(s.prs.as_deref(), visible.prs),
                &q,
                visible.queue.is_some(),
            ),
            visible.queue.map_or_else(Vec::new, |mode| {
                s.queue.as_deref().map_or_else(Vec::new, |rows| {
                    mode.iter(rows)
                        .filter(|row| hit(&row.haystack(), &q))
                        .map(Searchable::url)
                        .collect()
                })
            }),
            visible.merged.map_or_else(Vec::new, |shown| {
                group(
                    s.merged
                        .as_deref()
                        .map(|rows| &rows[..shown.min(rows.len())]),
                    &q,
                )
            }),
            if visible.shipments {
                shipments(s, &q)
            } else {
                Vec::new()
            },
        ],
        View::Reviews => vec![
            group(when_visible(s.reviews.as_deref(), visible.reviews), &q),
            group(
                when_visible(s.reviewed_merged.as_deref(), visible.reviewed_merged),
                &q,
            ),
        ],
    }
}

/// The open URL of every navigable row in `view` that matches `query`, in the
/// exact top-to-bottom order the dashboard renders them, so a selection index
/// lines up with the rendered (and identically filtered) rows. Rows without a
/// URL (an "upcoming" shipments row with no commits) are skipped. An empty
/// `query` yields every row.
#[cfg(test)]
fn targets<'a>(view: View, s: &'a Sections, query: &str) -> Vec<&'a str> {
    targets_visible(view, s, query, Visibility::all(s))
}

pub(crate) fn targets_visible<'a>(
    view: View,
    s: &'a Sections,
    query: &str,
    visible: Visibility,
) -> Vec<&'a str> {
    groups(view, s, query, visible).concat()
}

pub(crate) fn target_index(
    view: View,
    s: &Sections,
    query: &str,
    visible: Visibility,
    url: &str,
) -> Option<usize> {
    targets_visible(view, s, query, visible)
        .iter()
        .position(|target| *target == url)
}

/// Every target of the section containing the `index`-th target — what `Y`
/// copies. Empty sections hold no index, so passing 0 with no selection yields
/// the first non-empty section; an out-of-range index yields nothing.
#[cfg(test)]
fn section_at<'a>(view: View, s: &'a Sections, query: &str, index: usize) -> Vec<&'a str> {
    section_at_visible(view, s, query, index, Visibility::all(s))
}

pub(crate) fn section_at_visible<'a>(
    view: View,
    s: &'a Sections,
    query: &str,
    index: usize,
    visible: Visibility,
) -> Vec<&'a str> {
    let mut start = 0;
    for g in groups(view, s, query, visible) {
        if index < start + g.len() {
            return g;
        }
        start += g.len();
    }
    Vec::new()
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
    rows.iter()
        .filter(|row| hit(&row.haystack(), query))
        .cloned()
        .collect()
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
        releases: stats
            .releases
            .iter()
            .filter(|release| hit(&release.tag, query_lower))
            .cloned()
            .collect(),
    }
}

/// A selection movement, decoupled from the key that produced it (the input
/// layer maps keys to these).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Move {
    Up,
    Down,
    Top,
    Bottom,
    HalfUp,
    HalfDown,
}

/// The new selection after a [`Move`] against a `len`-row list (`half` is the
/// half-page step). From no selection, any move enters at the top except
/// `Bottom`, which enters at the last row.
pub(crate) fn moved(action: Move, sel: Option<usize>, len: usize, half: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let last = len - 1;
    let step = half.max(1);
    Some(match action {
        Move::Top => 0,
        Move::Bottom => last,
        Move::Up => sel.map_or(0, |i| i.saturating_sub(1)),
        Move::Down => sel.map_or(0, |i| (i + 1).min(last)),
        Move::HalfUp => sel.map_or(0, |i| i.saturating_sub(step)),
        Move::HalfDown => sel.map_or(0, |i| (i + step).min(last)),
    })
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
            branch: format!("b/{n}"),
            approval: crate::status::Approval::Approved,
            conflicts: false,
            status: None,
            checks: crate::status::Checks::default(),
            unresolved: 0,
            unresolved_capped: false,
            queue: None,
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
            branch: format!("b/{n}"),
            url: format!("https://q/{n}"),
            mine: true,
            enqueued_at: None,
            build_started_at: None,
            checks: crate::status::Checks::default(),
        }
    }

    fn merged(n: i64) -> MergedRow {
        MergedRow {
            number: n,
            title: format!("m {n}"),
            branch: format!("b/{n}"),
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
            branch: "b/r".into(),
            author: "a".into(),
            url: "https://rev/1".into(),
            state: ReviewState::Awaiting,
            updated_at: None,
        }]);
        s.reviewed_merged = Some(vec![ReviewedMergedRow {
            number: 2,
            title: "rm".into(),
            branch: "b/rm".into(),
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
    fn hidden_sections_are_not_navigation_targets() {
        let mut s = empty();
        s.prs = Some(vec![pr(1)]);
        s.queue = Some(vec![queued(2)]);
        s.merged = Some(vec![merged(3)]);
        let visible = Visibility {
            prs: true,
            queue: None,
            merged: None,
            shipments: false,
            reviews: false,
            reviewed_merged: false,
        };
        assert_eq!(
            targets_visible(View::Mine, &s, "", visible),
            vec!["https://pr/1"]
        );
        assert_eq!(
            section_at_visible(View::Mine, &s, "", 0, visible),
            vec!["https://pr/1"]
        );
        assert_eq!(
            target_index(View::Mine, &s, "", visible, "https://pr/1"),
            Some(0)
        );
        assert_eq!(
            target_index(View::Mine, &s, "", visible, "https://q/2"),
            None
        );
    }

    #[test]
    fn partial_sections_expose_only_rendered_targets() {
        let mut building = queued(1);
        building.mine = false;
        building.checks.running = 1;
        let mine = queued(2);
        let mut other = queued(3);
        other.mine = false;

        let mut s = empty();
        s.queue = Some(vec![building, mine, other]);
        s.merged = Some(vec![merged(4), merged(5)]);
        let mut visible = Visibility::all(&s);
        visible.queue = Some(crate::queue::VisibleRows::limited(
            s.queue.as_deref().unwrap(),
            2,
        ));
        visible.merged = Some(1);

        assert_eq!(
            targets_visible(View::Mine, &s, "", visible),
            vec!["https://q/1", "https://q/2", "https://m/4"]
        );
        assert_eq!(
            section_at_visible(View::Mine, &s, "", 0, visible),
            vec!["https://q/1", "https://q/2"]
        );
        assert_eq!(
            section_at_visible(View::Mine, &s, "", 2, visible),
            vec!["https://m/4"]
        );

        visible.queue = Some(crate::queue::VisibleRows::limited(
            s.queue.as_deref().unwrap(),
            1,
        ));
        assert_eq!(
            targets_visible(View::Mine, &s, "", visible),
            vec!["https://q/1", "https://m/4"]
        );
    }

    #[test]
    fn queue_membership_changes_can_restore_selection_by_url() {
        let mut building = queued(1);
        building.mine = false;
        building.checks.running = 1;
        let mine_b = queued(2);
        let mine_c = queued(3);
        let mut s = empty();
        s.queue = Some(vec![building, mine_b, mine_c]);
        let mut visible = Visibility::all(&s);
        visible.queue = Some(crate::queue::VisibleRows::limited(
            s.queue.as_deref().unwrap(),
            2,
        ));

        let selected = targets_visible(View::Mine, &s, "", visible)[1].to_string();
        s.queue.as_mut().unwrap()[0].checks.running = 0;
        visible.queue = Some(crate::queue::VisibleRows::limited(
            s.queue.as_deref().unwrap(),
            2,
        ));

        assert_eq!(
            target_index(View::Mine, &s, "", visible, &selected),
            Some(0),
            "the same PR should move with its URL, not retain its old index"
        );
    }

    #[test]
    fn queued_pr_returns_to_open_targets_when_queue_is_hidden() {
        let mut queued_pr = pr(1);
        queued_pr.queue = Some((1, "QUEUED".into()));
        let mut s = empty();
        s.prs = Some(vec![queued_pr]);
        s.queue = Some(vec![queued(1)]);

        let mut visible = Visibility::all(&s);
        assert_eq!(
            targets_visible(View::Mine, &s, "", visible),
            vec!["https://q/1"]
        );
        visible.queue = None;
        assert_eq!(
            targets_visible(View::Mine, &s, "", visible),
            vec!["https://pr/1"]
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
    fn section_at_returns_the_whole_section_under_the_cursor() {
        let mut s = empty();
        s.prs = Some(vec![pr(1), pr(2)]);
        s.queue = Some(vec![queued(3)]);
        s.merged = Some(vec![merged(4), merged(5)]);
        s.commits = Some(CommitStats {
            available: true,
            upcoming: Some(bucket("https://up")),
            releases: vec![Release {
                tag: "v1".into(),
                bucket: bucket("https://rel/v1"),
                published_at: None,
            }],
        });
        // Indices 0-1 are the open PRs, 2 the queue, 3-4 merged, 5-6 shipments.
        let at = |i| section_at(View::Mine, &s, "", i);
        assert_eq!(at(0), vec!["https://pr/1", "https://pr/2"]);
        assert_eq!(at(1), vec!["https://pr/1", "https://pr/2"]);
        assert_eq!(at(2), vec!["https://q/3"]);
        assert_eq!(at(4), vec!["https://m/4", "https://m/5"]);
        assert_eq!(at(6), vec!["https://up", "https://rel/v1"]);
        // Past the end there's no section.
        assert!(at(7).is_empty());
    }

    #[test]
    fn section_at_skips_empty_sections_and_honors_the_filter() {
        let mut s = empty();
        // No open PRs: index 0 must land on the first *non-empty* section, so a
        // `Y` with no selection copies the queue rather than nothing.
        s.prs = Some(vec![]);
        s.queue = Some(vec![queued(3)]);
        s.merged = Some(vec![merged(4)]);
        assert_eq!(section_at(View::Mine, &s, "", 0), vec!["https://q/3"]);

        // The filter applies to the section's contents too: only matching rows.
        let mut s = empty();
        s.prs = Some(vec![pr(1), pr(2)]);
        assert_eq!(section_at(View::Mine, &s, "#2", 0), vec!["https://pr/2"]);
        assert!(section_at(View::Mine, &s, "zzz", 0).is_empty());
    }

    #[test]
    fn section_at_groups_the_reviews_view() {
        let mut s = empty();
        s.reviews = Some(vec![ReviewRow {
            number: 1,
            is_draft: false,
            title: "r".into(),
            branch: "b/r".into(),
            author: "a".into(),
            url: "https://rev/1".into(),
            state: ReviewState::Awaiting,
            updated_at: None,
        }]);
        s.reviewed_merged = Some(vec![ReviewedMergedRow {
            number: 2,
            title: "rm".into(),
            branch: "b/rm".into(),
            author: "a".into(),
            url: "https://revm/2".into(),
            merged_at: None,
        }]);
        assert_eq!(section_at(View::Reviews, &s, "", 0), vec!["https://rev/1"]);
        assert_eq!(section_at(View::Reviews, &s, "", 1), vec!["https://revm/2"]);
    }

    #[test]
    fn movement_enters_and_clamps() {
        // From no selection, a down-ish move enters at the top; Bottom at the end.
        assert_eq!(moved(Move::Down, None, 5, 2), Some(0));
        assert_eq!(moved(Move::Up, None, 5, 2), Some(0));
        assert_eq!(moved(Move::Bottom, None, 5, 2), Some(4));
        // Stepping is clamped to the ends.
        assert_eq!(moved(Move::Down, Some(4), 5, 2), Some(4));
        assert_eq!(moved(Move::Up, Some(0), 5, 2), Some(0));
        assert_eq!(moved(Move::HalfDown, Some(0), 5, 2), Some(2));
        assert_eq!(moved(Move::HalfUp, Some(4), 5, 2), Some(2));
        assert_eq!(moved(Move::Top, Some(3), 5, 2), Some(0));
        // An empty list has nothing to select.
        assert_eq!(moved(Move::Down, Some(0), 0, 2), None);
    }
}

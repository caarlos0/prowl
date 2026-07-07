//! Reviews view: open PRs awaiting or under my review (with a per-row review
//! state glyph), and merged PRs I reviewed. The state glyphs/colors live in the
//! shared `status` palette; this module only builds rows and tables.

use crate::model::{MergedNode, ReviewPrNode, ReviewsData};
use crate::render::{self, Cell, Table};
use crate::status::{self, BLUE, MAUVE, ReviewState, Status};
use crate::timefmt;
use anstyle::Style;
use std::collections::{HashMap, HashSet};

/// Author logins are truncated to this many display columns (matches the queue).
const AUTHOR_WIDTH: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewRow {
    pub number: i64,
    pub is_draft: bool,
    pub title: String,
    pub author: String,
    pub url: String,
    pub state: ReviewState,
    pub updated_at: Option<String>,
}

/// Sort rank for a review state — most actionable first.
fn rank(s: ReviewState) -> usize {
    status::REVIEW_ORDER
        .iter()
        .position(|r| *r == s)
        .unwrap_or(0)
}

/// Whether the PR has new commits since my latest review (both timestamps are
/// RFC 3339 `…Z`, so a lexical compare is chronological).
fn updated_since(last_commit: Option<&str>, my_last_review: Option<&str>) -> bool {
    matches!((last_commit, my_last_review), (Some(c), Some(r)) if c > r)
}

/// Derive a PR's review state from whether it currently requests my review,
/// whether I've reviewed it, and (for reviewed PRs) whether it moved since.
fn review_state(requested: bool, node: &ReviewPrNode) -> ReviewState {
    let has_my_review = !node.reviews.nodes.is_empty();
    match (requested, has_my_review) {
        (true, true) => ReviewState::ReReview,
        (true, false) => ReviewState::Awaiting,
        (false, true) => {
            let last_commit = node
                .commits
                .nodes
                .first()
                .and_then(|c| c.commit.committed_date.as_deref());
            let my_last_review = node
                .reviews
                .nodes
                .iter()
                .filter_map(|r| r.submitted_at.as_deref())
                .max();
            if updated_since(last_commit, my_last_review) {
                ReviewState::Updated
            } else {
                ReviewState::Reviewed
            }
        }
        // A PR I haven't reviewed and that isn't requesting me shouldn't surface,
        // but treat it as quietly reviewed rather than panic.
        (false, false) => ReviewState::Reviewed,
    }
}

/// Build the open-reviews rows from the two searches, de-duplicating PRs that
/// appear in both (a re-review). Sorted by state (actionable first), then by
/// last update (most recent first).
pub fn build_open_rows(data: ReviewsData) -> Vec<ReviewRow> {
    let mut requested: HashSet<i64> = HashSet::new();
    let mut nodes: HashMap<i64, ReviewPrNode> = HashMap::new();
    for n in data.requested.nodes {
        requested.insert(n.number);
        nodes.entry(n.number).or_insert(n);
    }
    for n in data.reviewed.nodes {
        nodes.entry(n.number).or_insert(n);
    }

    let mut rows: Vec<ReviewRow> = nodes
        .into_values()
        .map(|n| {
            let state = review_state(requested.contains(&n.number), &n);
            ReviewRow {
                number: n.number,
                is_draft: n.is_draft,
                title: n.title,
                author: n.author.map_or_else(|| "ghost".into(), |a| a.login),
                url: n.url,
                state,
                updated_at: n.updated_at,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        rank(a.state)
            .cmp(&rank(b.state))
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| b.number.cmp(&a.number))
    });
    rows
}

pub fn open_to_table(rows: &[ReviewRow], ascii: bool) -> Table {
    let dim = Style::new().dimmed();
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let glyph = status::review_glyph(r.state, ascii);
        let st = Cell::styled(
            glyph.to_string(),
            status::fg(status::review_style(r.state).1),
        );
        let pr = if r.is_draft {
            Cell::link_styled(format!("#{}", r.number), r.url.clone(), dim)
        } else {
            Cell::link_styled(format!("#{}", r.number), r.url.clone(), status::fg(BLUE))
        };
        out.push(vec![
            // A leading (always-blank) margin column keeps the two reviews
            // tables aligned with each other and the rest of the dashboard.
            Cell::plain(" "),
            st,
            pr,
            Cell::plain(r.title.clone()),
            Cell::styled(render::truncate(&r.author, AUTHOR_WIDTH, ascii), dim),
            Cell::styled(timefmt::age_of(r.updated_at.as_deref()), dim),
        ]);
    }
    Table {
        header: vec!["", "", "PR", "TITLE", "AUTHOR", "UPDATED"],
        rows: out,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewedMergedRow {
    pub number: i64,
    pub title: String,
    pub author: String,
    pub url: String,
    pub merged_at: Option<String>,
}

/// Build the "reviewed & merged" rows sorted by merge time (most recent first),
/// capped at `limit`.
pub fn build_merged_rows(nodes: Vec<MergedNode>, limit: usize) -> Vec<ReviewedMergedRow> {
    let mut rows: Vec<ReviewedMergedRow> = nodes
        .into_iter()
        .map(|n| ReviewedMergedRow {
            number: n.number,
            title: n.title,
            author: n.author.map_or_else(|| "ghost".into(), |a| a.login),
            url: n.url,
            merged_at: n.merged_at,
        })
        .collect();
    rows.sort_by(|a, b| {
        b.merged_at
            .cmp(&a.merged_at)
            .then_with(|| b.number.cmp(&a.number))
    });
    rows.truncate(limit);
    rows
}

pub fn merged_to_table(rows: &[ReviewedMergedRow], ascii: bool) -> Table {
    let glyph = status::glyph(Status::Merged, ascii);
    let dim = Style::new().dimmed();
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(vec![
            Cell::plain(" "),
            Cell::styled(glyph.to_string(), status::fg(MAUVE)),
            Cell::link_styled(format!("#{}", r.number), r.url.clone(), status::fg(BLUE)),
            Cell::plain(r.title.clone()),
            Cell::styled(render::truncate(&r.author, AUTHOR_WIDTH, ascii), dim),
            Cell::styled(timefmt::age_of(r.merged_at.as_deref()), dim),
        ]);
    }
    Table {
        header: vec!["", "", "PR", "TITLE", "AUTHOR", "MERGED"],
        rows: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Login, MyReview, MyReviews, ReviewCommit, ReviewCommitNode, ReviewCommits, ReviewPrNode,
        ReviewSearch, ReviewsData,
    };

    fn node(
        number: i64,
        author: &str,
        last_commit: Option<&str>,
        my_reviews: &[&str],
    ) -> ReviewPrNode {
        ReviewPrNode {
            number,
            title: format!("PR {number}"),
            url: format!("https://x/{number}"),
            is_draft: false,
            updated_at: Some("2026-06-19T00:00:00Z".to_string()),
            author: Some(Login {
                login: author.to_string(),
            }),
            commits: ReviewCommits {
                nodes: last_commit
                    .map(|c| ReviewCommitNode {
                        commit: ReviewCommit {
                            committed_date: Some(c.to_string()),
                        },
                    })
                    .into_iter()
                    .collect(),
            },
            reviews: MyReviews {
                nodes: my_reviews
                    .iter()
                    .map(|s| MyReview {
                        submitted_at: Some(s.to_string()),
                    })
                    .collect(),
            },
        }
    }

    #[test]
    fn derives_all_review_states_and_orders_them() {
        let data = ReviewsData {
            // #1 awaiting (requested, never reviewed); #2 re-review (requested + reviewed).
            requested: ReviewSearch {
                nodes: vec![
                    node(1, "alice", Some("2026-06-10T00:00:00Z"), &[]),
                    node(
                        2,
                        "bob",
                        Some("2026-06-10T00:00:00Z"),
                        &["2026-06-09T00:00:00Z"],
                    ),
                ],
            },
            // #2 also here (re-review); #3 updated (commit after review); #4 reviewed.
            reviewed: ReviewSearch {
                nodes: vec![
                    node(
                        2,
                        "bob",
                        Some("2026-06-10T00:00:00Z"),
                        &["2026-06-09T00:00:00Z"],
                    ),
                    node(
                        3,
                        "carol",
                        Some("2026-06-12T00:00:00Z"),
                        &["2026-06-08T00:00:00Z"],
                    ),
                    node(
                        4,
                        "dave",
                        Some("2026-06-01T00:00:00Z"),
                        &["2026-06-05T00:00:00Z"],
                    ),
                ],
            },
        };
        let rows = build_open_rows(data);
        // De-duplicated to four unique PRs.
        assert_eq!(rows.len(), 4);
        // Ordered by state rank: Awaiting, ReReview, Updated, Reviewed.
        assert_eq!(
            rows.iter().map(|r| (r.number, r.state)).collect::<Vec<_>>(),
            vec![
                (1, ReviewState::Awaiting),
                (2, ReviewState::ReReview),
                (3, ReviewState::Updated),
                (4, ReviewState::Reviewed),
            ]
        );
    }

    #[test]
    fn reviewed_without_new_commits_is_not_updated() {
        // Commit predates my review -> Reviewed, not Updated.
        let data = ReviewsData {
            requested: ReviewSearch { nodes: vec![] },
            reviewed: ReviewSearch {
                nodes: vec![node(
                    9,
                    "eve",
                    Some("2026-06-01T00:00:00Z"),
                    &["2026-06-10T00:00:00Z"],
                )],
            },
        };
        let rows = build_open_rows(data);
        assert_eq!(rows[0].state, ReviewState::Reviewed);
    }

    #[test]
    fn open_table_uses_ascii_glyphs_and_author() {
        let data = ReviewsData {
            requested: ReviewSearch {
                nodes: vec![node(1, "alice", None, &[])],
            },
            reviewed: ReviewSearch { nodes: vec![] },
        };
        let rows = build_open_rows(data);
        let table = open_to_table(&rows, true);
        // Columns: [margin, glyph, PR, TITLE, AUTHOR, UPDATED].
        assert_eq!(table.rows[0][1].text, "a"); // Awaiting ASCII glyph
        assert_eq!(table.rows[0][2].text, "#1");
        assert_eq!(table.rows[0][4].text, "alice");
    }

    fn merged_node(number: i64, author: &str, merged_at: &str) -> MergedNode {
        MergedNode {
            number,
            title: format!("PR {number}"),
            url: format!("https://x/{number}"),
            author: Some(Login {
                login: author.to_string(),
            }),
            merged_at: Some(merged_at.to_string()),
        }
    }

    #[test]
    fn merged_rows_sort_desc_and_cap() {
        let rows = build_merged_rows(
            vec![
                merged_node(1, "a", "2026-06-10T00:00:00Z"),
                merged_node(2, "b", "2026-06-18T00:00:00Z"),
                merged_node(3, "c", "2026-06-14T00:00:00Z"),
            ],
            2,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].number, 2);
        assert_eq!(rows[0].author, "b");
        assert_eq!(rows[1].number, 3);
    }
}

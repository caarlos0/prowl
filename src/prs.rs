//! My-open-PRs view: rows, sorting, styling, and table building.
//!
//! One row is: a change marker, an approval glyph, the PR number + title +
//! branch, and then the detail group that explains a PR that cannot merge yet —
//! a failing/running/passing check semaphore and the unresolved-review-thread
//! count. A conflicting PR marks its own title, so conflicts cost no column.
//! Each column answers exactly one question, so nothing is reported twice.

use crate::cli::OpenSort;
use crate::model::PrNode;
use crate::render::{self, Cell, Table};
use crate::status::{self, Approval, BLUE, Checks, Lamp, PEACH, RED, Status};
use std::collections::HashSet;
use uncurses::style::Style;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrRow {
    pub number: i64,
    pub is_draft: bool,
    pub title: String,
    /// Head branch.
    #[serde(default)]
    pub branch: String,
    /// The leading glyph: any approval or all required approvals.
    pub approval: Approval,
    /// Whether it conflicts with its base branch — marks the title.
    pub conflicts: bool,
    /// Coarse CI/merge state; not rendered, it is the bell's change key.
    pub status: Option<Status>,
    /// Failing / pending / passing checks, including unreported required checks.
    pub checks: Checks,
    /// Unresolved review threads (capped at one page — see `unresolved_capped`).
    pub unresolved: usize,
    /// Whether the PR has more review threads than the page we counted.
    pub unresolved_capped: bool,
    pub queue: Option<(i64, String)>,
    pub url: String,
    pub updated_at: Option<String>,
    pub created_at: Option<String>,
}

/// Build rows sorted by the chosen timestamp (most recent first).
pub fn build_rows(nodes: Vec<PrNode>, sort: OpenSort) -> Vec<PrRow> {
    let mut rows: Vec<PrRow> = nodes
        .into_iter()
        .map(|pr| {
            let checks = pr.checks();
            let conflicts =
                status::conflicts_of(pr.merge_state_status.as_deref(), pr.mergeable.as_deref());
            let (unresolved, unresolved_capped) = pr.review_threads.unresolved();
            PrRow {
                number: pr.number,
                is_draft: pr.is_draft,
                approval: pr.approval(),
                conflicts,
                status: status::derive_status(conflicts, checks),
                checks,
                unresolved,
                unresolved_capped,
                queue: pr.merge_queue_entry.map(|e| (e.position, e.state)),
                title: pr.title,
                branch: pr.head_ref_name.unwrap_or_default(),
                url: pr.url,
                updated_at: pr.updated_at,
                created_at: pr.created_at,
            }
        })
        .collect();
    sort_rows(&mut rows, sort);
    rows
}

pub(crate) fn sort_rows(rows: &mut [PrRow], sort: OpenSort) {
    rows.sort_by(|a, b| {
        let order = match sort {
            OpenSort::Updated => b.updated_at.cmp(&a.updated_at),
            OpenSort::Created => b.created_at.cmp(&a.created_at),
        };
        order.then_with(|| b.number.cmp(&a.number))
    });
}

/// Drop PRs that are in the merge queue: they're shown in the Merge Queue
/// section, so listing them here too would be redundant. Kept separate from
/// `build_rows` so the caller can skip it when the queue section is hidden (and
/// the PR would otherwise vanish entirely).
pub fn without_queued(mut rows: Vec<PrRow>) -> Vec<PrRow> {
    rows.retain(|r| r.queue.is_none());
    rows
}

/// Drop draft PRs (`--no-draft`).
pub fn without_drafts(mut rows: Vec<PrRow>) -> Vec<PrRow> {
    rows.retain(|r| !r.is_draft);
    rows
}

pub fn to_table(rows: &[PrRow], ascii: bool, highlight: &HashSet<i64>, show_branch: bool) -> Table {
    let dim = Style::new().faint();
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let mark = render::change_marker(highlight.contains(&r.number), ascii);
        let approval = Cell::styled(
            status::approval_glyph(r.approval, ascii).to_string(),
            status::fg(status::approval_style(r.approval).1),
        );
        // A conflicting PR marks its own title, in red, instead of spending a
        // column that every other row would leave blank.
        let title = if r.conflicts {
            Cell::styled(
                format!("{} {}", status::conflict_marker(ascii), r.title),
                status::fg(RED),
            )
        } else {
            Cell::plain(r.title.clone())
        };
        // A draft's number is dimmed: it can't merge whatever the glyph says.
        let pr_style = if r.is_draft {
            dim.clone()
        } else {
            status::fg(BLUE)
        };
        let pr = Cell::pr(r.number, r.url.clone(), &pr_style);
        let threads = if r.unresolved == 0 {
            Cell::styled("0".to_string(), &dim)
        } else {
            let capped = if r.unresolved_capped { "+" } else { "" };
            Cell::styled(
                format!("{}{capped}", r.unresolved),
                status::fg(PEACH).bold(),
            )
        };

        let mut row = vec![mark, approval, pr, title];
        if show_branch {
            row.push(Cell::styled(r.branch.clone(), &dim));
        }
        row.extend([
            threads,
            render::lamp_cell(r.checks.fail, Lamp::Fail),
            render::lamp_cell(r.checks.running, Lamp::Running),
            render::lamp_cell(r.checks.pass, Lamp::Pass),
        ]);
        out.push(row);
    }
    let mut header = vec!["", "", "PR", "TITLE"];
    if show_branch {
        header.push("BRANCH");
    }
    header.extend(["THREADS", "FAIL", "RUN", "PASS"]);
    Table { header, rows: out }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Commit, CommitNode, Commits, OpinionatedReview, OpinionatedReviews, QueueEntry,
        ReviewThread, ReviewThreads, Rollup, RollupCounts, StateCount,
    };

    /// A PR node with the given merge state and per-state check-run counts.
    fn pr(number: i64, mergeable: &str, state: &str, runs: &[(&str, u64)]) -> PrNode {
        PrNode {
            number,
            title: format!("PR {number}"),
            url: format!("https://x/{number}"),
            mergeable: Some(mergeable.to_string()),
            merge_state_status: Some(state.to_string()),
            is_draft: false,
            updated_at: None,
            created_at: None,
            head_ref_name: Some(format!("branch-{number}")),
            base_ref: None,
            review_decision: None,
            latest_opinionated_reviews: OpinionatedReviews::default(),
            merge_queue_entry: None,
            review_threads: ReviewThreads {
                total_count: 0,
                nodes: vec![],
            },
            commits: Commits {
                nodes: vec![CommitNode {
                    commit: Commit {
                        id: format!("COMMIT_{number}"),
                        status_check_rollup: Some(Rollup {
                            contexts: RollupCounts {
                                check_runs: runs
                                    .iter()
                                    .map(|(state, count)| StateCount {
                                        state: (*state).to_string(),
                                        count: *count,
                                    })
                                    .collect(),
                                status_contexts: vec![],
                            },
                        }),
                    },
                }],
            },
            required_checks: None,
            missing_required_checks: 0,
        }
    }

    #[test]
    fn sorts_by_updated_at_then_derives_checks_and_conflicts() {
        let mut a = pr(10, "MERGEABLE", "BLOCKED", &[("SUCCESS", 8)]);
        a.updated_at = Some("2026-06-19T10:00:00Z".to_string());
        let mut b = pr(
            42,
            "CONFLICTING",
            "DIRTY",
            &[("FAILURE", 2), ("IN_PROGRESS", 1), ("SUCCESS", 3)],
        );
        b.updated_at = Some("2026-06-19T09:00:00Z".to_string());
        // #10 was updated more recently than #42, so it sorts first despite the
        // lower number.
        let rows = build_rows(vec![a, b], OpenSort::Updated);
        assert_eq!(rows[0].number, 10);
        assert!(!rows[0].conflicts);
        assert_eq!(
            rows[0].checks,
            Checks {
                fail: 0,
                running: 0,
                pass: 8
            }
        );
        assert_eq!(rows[0].status, Some(Status::Pass));
        assert_eq!(rows[1].number, 42);
        assert!(rows[1].conflicts);
        assert_eq!(
            rows[1].checks,
            Checks {
                fail: 2,
                running: 1,
                pass: 3
            }
        );
        assert_eq!(rows[1].status, Some(Status::Conflicts));
    }

    #[test]
    fn created_sort_uses_creation_time_not_pr_number_or_updates() {
        let mut older = pr(20, "MERGEABLE", "CLEAN", &[]);
        older.created_at = Some("2026-01-01T00:00:00Z".into());
        older.updated_at = Some("2026-03-02T00:00:00Z".into());
        let mut newer = pr(10, "MERGEABLE", "CLEAN", &[]);
        newer.created_at = Some("2026-01-02T00:00:00Z".into());
        newer.updated_at = Some("2026-03-01T00:00:00Z".into());
        let mut rows = build_rows(vec![older, newer], OpenSort::Created);
        assert_eq!(
            rows.iter().map(|row| row.number).collect::<Vec<_>>(),
            [10, 20]
        );
        assert_eq!(rows[0].created_at.as_deref(), Some("2026-01-02T00:00:00Z"));

        rows[1].updated_at = Some("2026-04-01T00:00:00Z".into());
        sort_rows(&mut rows, OpenSort::Created);
        assert_eq!(
            rows.iter().map(|row| row.number).collect::<Vec<_>>(),
            [10, 20]
        );
        sort_rows(&mut rows, OpenSort::Updated);
        assert_eq!(
            rows.iter().map(|row| row.number).collect::<Vec<_>>(),
            [20, 10]
        );
    }

    #[test]
    fn both_sorts_break_ties_by_number_and_put_missing_timestamps_last() {
        for sort in [OpenSort::Updated, OpenSort::Created] {
            let nodes = (1..=4)
                .map(|number| {
                    let mut node = pr(number, "MERGEABLE", "CLEAN", &[]);
                    if number <= 2 {
                        node.updated_at = Some("2026-01-01T00:00:00Z".into());
                        node.created_at = node.updated_at.clone();
                    }
                    node
                })
                .collect();
            let rows = build_rows(nodes, sort);
            assert_eq!(
                rows.iter().map(|row| row.number).collect::<Vec<_>>(),
                [2, 1, 4, 3],
                "{sort:?}"
            );
            assert!(build_rows(vec![], sort).is_empty());
        }
    }

    #[test]
    fn required_mode_uses_only_required_check_counts() {
        let mut p = pr(1, "MERGEABLE", "BLOCKED", &[("FAILURE", 2), ("SUCCESS", 8)]);
        p.required_checks = Some(Checks {
            fail: 0,
            running: 1,
            pass: 2,
        });

        let rows = build_rows(vec![p], OpenSort::Updated);
        assert_eq!(
            rows[0].checks,
            Checks {
                fail: 0,
                running: 1,
                pass: 2,
            }
        );
        assert_eq!(rows[0].status, Some(Status::Pending));
    }

    /// A PR whose reviewers left these latest opinionated review states.
    fn reviewed(number: i64, states: &[&str]) -> PrNode {
        let mut p = pr(number, "MERGEABLE", "CLEAN", &[]);
        p.latest_opinionated_reviews = OpinionatedReviews {
            nodes: states
                .iter()
                .map(|state| OpinionatedReview {
                    state: (*state).to_string(),
                })
                .collect(),
        };
        p
    }

    #[test]
    fn approval_comes_from_the_latest_reviews() {
        let rows = build_rows(
            vec![
                reviewed(1, &["APPROVED"]),
                // A change request does not undo an approval: THREADS reports what
                // is still open.
                reviewed(2, &["APPROVED", "CHANGES_REQUESTED"]),
                reviewed(3, &["CHANGES_REQUESTED"]),
                reviewed(4, &[]),
            ],
            OpenSort::Updated,
        );
        let approval = |number: i64| {
            rows.iter()
                .find(|r| r.number == number)
                .expect("row")
                .approval
        };
        assert_eq!(approval(1), Approval::Approved);
        assert_eq!(approval(2), Approval::Approved);
        assert_eq!(approval(3), Approval::Pending);
        assert_eq!(approval(4), Approval::Pending);
    }

    #[test]
    fn approval_leads_the_row_and_a_conflict_marks_the_title() {
        let mut conflicted = reviewed(1, &["APPROVED"]);
        conflicted.mergeable = Some("CONFLICTING".to_string());
        conflicted.merge_state_status = Some("DIRTY".to_string());
        let rows = build_rows(vec![conflicted], OpenSort::Updated);
        let table = to_table(&rows, true, &HashSet::new(), false);
        // [mark] [approval] PR TITLE ...
        assert_eq!(table.rows[0][1].text, "y");
        assert_eq!(table.rows[0][3].text, "! PR 1");
    }

    #[test]
    fn a_clean_title_carries_no_marker() {
        let rows = build_rows(vec![pr(1, "MERGEABLE", "CLEAN", &[])], OpenSort::Updated);
        let table = to_table(&rows, true, &HashSet::new(), false);
        assert_eq!(table.rows[0][3].text, "PR 1");
    }

    #[test]
    fn counts_unresolved_review_threads() {
        let mut p = pr(1, "MERGEABLE", "CLEAN", &[]);
        p.review_threads = ReviewThreads {
            total_count: 3,
            nodes: vec![
                ReviewThread { is_resolved: true },
                ReviewThread { is_resolved: false },
                ReviewThread { is_resolved: false },
            ],
        };
        let rows = build_rows(vec![p], OpenSort::Updated);
        assert_eq!(rows[0].unresolved, 2);
        assert!(!rows[0].unresolved_capped);
    }

    #[test]
    fn flags_a_truncated_review_thread_page() {
        let mut p = pr(1, "MERGEABLE", "CLEAN", &[]);
        // The server reports 120 threads but we only fetched one page of 100.
        p.review_threads = ReviewThreads {
            total_count: 120,
            nodes: (0..100)
                .map(|_| ReviewThread { is_resolved: false })
                .collect(),
        };
        let rows = build_rows(vec![p], OpenSort::Updated);
        assert_eq!(rows[0].unresolved, 100);
        assert!(rows[0].unresolved_capped);
        let table = to_table(&rows, true, &HashSet::new(), false);
        let threads = table
            .header
            .iter()
            .position(|header| *header == "THREADS")
            .expect("THREADS column");
        assert_eq!(table.rows[0][threads].text, "100+");
    }

    #[test]
    fn a_commit_without_a_rollup_has_no_checks() {
        let mut p = pr(1, "MERGEABLE", "CLEAN", &[]);
        p.commits.nodes[0].commit.status_check_rollup = None;
        let rows = build_rows(vec![p], OpenSort::Updated);
        assert!(rows[0].checks.is_empty());
        assert_eq!(rows[0].status, None);
    }

    #[test]
    fn queue_entry_becomes_position_and_state() {
        let mut p = pr(1, "MERGEABLE", "CLEAN", &[("SUCCESS", 1)]);
        p.merge_queue_entry = Some(QueueEntry {
            position: 3,
            state: "QUEUED".to_string(),
        });
        let rows = build_rows(vec![p], OpenSort::Updated);
        assert_eq!(rows[0].queue, Some((3, "QUEUED".to_string())));
    }

    #[test]
    fn without_queued_drops_prs_in_the_merge_queue() {
        let mut queued = pr(1, "MERGEABLE", "CLEAN", &[("SUCCESS", 1)]);
        queued.merge_queue_entry = Some(QueueEntry {
            position: 1,
            state: "QUEUED".to_string(),
        });
        let open = pr(2, "MERGEABLE", "CLEAN", &[("SUCCESS", 1)]);
        // #1 is queued, #2 isn't — only #2 remains in the open-PRs list.
        let rows = without_queued(build_rows(vec![queued, open], OpenSort::Updated));
        assert_eq!(rows.iter().map(|r| r.number).collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn without_drafts_drops_draft_prs() {
        let mut draft = pr(1, "MERGEABLE", "DRAFT", &[]);
        draft.is_draft = true;
        let ready = pr(2, "MERGEABLE", "CLEAN", &[("SUCCESS", 1)]);
        let rows = without_drafts(build_rows(vec![draft, ready], OpenSort::Updated));
        assert_eq!(rows.iter().map(|r| r.number).collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn branch_column_is_opt_in() {
        let rows = build_rows(
            vec![pr(1, "MERGEABLE", "CLEAN", &[("SUCCESS", 1)])],
            OpenSort::Updated,
        );
        let table = to_table(&rows, true, &HashSet::new(), false);
        assert!(!table.header.contains(&"BRANCH"));
        let table = to_table(&rows, true, &HashSet::new(), true);
        assert_eq!(
            table.header,
            [
                "", "", "PR", "TITLE", "BRANCH", "THREADS", "FAIL", "RUN", "PASS"
            ]
        );
        assert_eq!(table.rows[0][4].text, "branch-1");
    }

    #[test]
    fn semaphore_shows_all_three_counts() {
        let rows = build_rows(
            vec![pr(
                1,
                "MERGEABLE",
                "CLEAN",
                &[
                    ("FAILURE", 2),
                    ("QUEUED", 1),
                    ("IN_PROGRESS", 2),
                    ("SUCCESS", 9),
                ],
            )],
            OpenSort::Updated,
        );
        let table = to_table(&rows, true, &HashSet::new(), false);
        // ..., THREADS, FAIL, RUN, PASS
        let tail: Vec<&str> = table.rows[0][4..].iter().map(|c| c.text.as_str()).collect();
        assert_eq!(tail, ["0", "2", "3", "9"]);
    }
}

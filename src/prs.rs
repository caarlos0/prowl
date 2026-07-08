//! My-open-PRs view: rows, sorting, styling, and table building. The `ST`
//! column is the shared status glyph; remaining color-coding uses the shared
//! Catppuccin palette.

use crate::model::PrNode;
use crate::render::{self, Cell, Table};
use crate::status::{self, BLUE, RED, Status};
use anstyle::Style;
use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrRow {
    pub number: i64,
    pub is_draft: bool,
    pub title: String,
    pub status: Option<Status>,
    pub merge_state: Option<String>,
    pub queue: Option<(i64, String)>,
    pub fail: usize,
    pub url: String,
    pub updated_at: Option<String>,
}

/// Build rows sorted by last update time (most recent first).
pub fn build_rows(nodes: Vec<PrNode>) -> Vec<PrRow> {
    let mut rows: Vec<PrRow> = nodes
        .into_iter()
        .map(|pr| {
            let status = status::pr_status(&pr);
            let fail = status::fail_count(status::last_suites(&pr));
            PrRow {
                number: pr.number,
                is_draft: pr.is_draft,
                status,
                merge_state: pr.merge_state_status,
                queue: pr.merge_queue_entry.map(|e| (e.position, e.state)),
                fail,
                title: pr.title,
                url: pr.url,
                updated_at: pr.updated_at,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.number.cmp(&a.number))
    });
    rows
}

/// Drop PRs that are in the merge queue: they're shown in the Merge Queue
/// section, so listing them here too would be redundant. Kept separate from
/// `build_rows` so the caller can skip it when the queue section is hidden (and
/// the PR would otherwise vanish entirely).
pub fn without_queued(mut rows: Vec<PrRow>) -> Vec<PrRow> {
    rows.retain(|r| r.queue.is_none());
    rows
}

pub fn to_table(rows: &[PrRow], ascii: bool, highlight: &HashSet<i64>) -> Table {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let mark = render::change_marker(highlight.contains(&r.number), ascii);
        let st = match r.status {
            Some(s) => Cell::styled(
                status::glyph(s, ascii).to_string(),
                status::fg(status::status_style(s).1),
            ),
            None => Cell::styled("-".to_string(), Style::new().dimmed()),
        };
        let pr = if r.is_draft {
            Cell::link_styled(
                format!("#{}", r.number),
                r.url.clone(),
                Style::new().dimmed(),
            )
        } else {
            Cell::link_styled(format!("#{}", r.number), r.url.clone(), status::fg(BLUE))
        };
        let state_raw = r.merge_state.clone().unwrap_or_else(|| "?".to_string());
        let state_text = if ascii {
            status::state_label(&state_raw).to_string()
        } else {
            status::state_glyph(&state_raw).to_string()
        };
        let state = Cell::styled(state_text, status::state_style(&state_raw));
        let (fail_text, fail_style) = if r.fail == 0 {
            ("-".to_string(), Style::new().dimmed())
        } else {
            (r.fail.to_string(), status::fg(RED).bold())
        };
        out.push(vec![
            mark,
            st,
            pr,
            Cell::plain(r.title.clone()),
            state,
            Cell::styled(fail_text, fail_style),
        ]);
    }
    Table {
        header: vec!["", "", "PR", "TITLE", "STATE", "FAIL"],
        rows: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CheckSuite, Commit, CommitNode, Commits, QueueEntry};

    fn pr(number: i64, mergeable: &str, state: &str, concls: &[Option<&str>]) -> PrNode {
        PrNode {
            number,
            title: format!("PR {number}"),
            url: format!("https://x/{number}"),
            state: Some("OPEN".to_string()),
            mergeable: Some(mergeable.to_string()),
            merge_state_status: Some(state.to_string()),
            is_draft: false,
            updated_at: None,
            merge_queue_entry: None,
            commits: Commits {
                nodes: vec![CommitNode {
                    commit: Commit {
                        check_suites: crate::model::CheckSuites {
                            total_count: concls.len() as u64,
                            nodes: concls
                                .iter()
                                .map(|c| CheckSuite {
                                    conclusion: c.map(str::to_string),
                                    check_runs: Some(crate::model::CheckRuns { total_count: 1 }),
                                })
                                .collect(),
                        },
                    },
                }],
            },
        }
    }

    #[test]
    fn sorts_by_updated_at_then_derives_status_and_fail() {
        let mut a = pr(10, "MERGEABLE", "BLOCKED", &[Some("SUCCESS")]);
        a.updated_at = Some("2026-06-19T10:00:00Z".to_string());
        let mut b = pr(
            42,
            "CONFLICTING",
            "DIRTY",
            &[Some("FAILURE"), Some("FAILURE")],
        );
        b.updated_at = Some("2026-06-19T09:00:00Z".to_string());
        // #10 was updated more recently than #42, so it sorts first despite the
        // lower number.
        let rows = build_rows(vec![a, b]);
        assert_eq!(rows[0].number, 10);
        assert_eq!(rows[0].status, Some(Status::Pass));
        assert_eq!(rows[0].fail, 0);
        assert_eq!(rows[1].number, 42);
        assert_eq!(rows[1].status, Some(Status::Conflicts));
        assert_eq!(rows[1].fail, 2);
    }

    #[test]
    fn queue_entry_becomes_position_and_state() {
        let mut p = pr(1, "MERGEABLE", "CLEAN", &[Some("SUCCESS")]);
        p.merge_queue_entry = Some(QueueEntry {
            position: 3,
            state: "QUEUED".to_string(),
        });
        let rows = build_rows(vec![p]);
        assert_eq!(rows[0].queue, Some((3, "QUEUED".to_string())));
    }

    #[test]
    fn without_queued_drops_prs_in_the_merge_queue() {
        let mut queued = pr(1, "MERGEABLE", "CLEAN", &[Some("SUCCESS")]);
        queued.merge_queue_entry = Some(QueueEntry {
            position: 1,
            state: "QUEUED".to_string(),
        });
        let open = pr(2, "MERGEABLE", "CLEAN", &[Some("SUCCESS")]);
        // #1 is queued, #2 isn't — only #2 remains in the open-PRs list.
        let rows = without_queued(build_rows(vec![queued, open]));
        assert_eq!(rows.iter().map(|r| r.number).collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn truncated_check_suites_degrade_pass_to_pending() {
        // The server reports 50 suites but we only fetched 1; a failing suite
        // could be hiding beyond the page, so the green is unproven -> pending.
        let mut p = pr(1, "MERGEABLE", "CLEAN", &[Some("SUCCESS")]);
        p.commits.nodes[0].commit.check_suites.total_count = 50;
        let rows = build_rows(vec![p]);
        assert_eq!(rows[0].status, Some(Status::Pending));
    }
}

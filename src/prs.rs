//! My-open-PRs view: rows, sorting, styling, and table building. The `ST`
//! column is the shared status glyph; remaining color-coding uses the shared
//! Catppuccin palette.

use crate::model::PrNode;
use crate::render::{Cell, Table};
use crate::status::{self, BLUE, GREEN, RED, Status, YELLOW};
use anstyle::Style;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrRow {
    pub number: i64,
    pub is_draft: bool,
    pub title: String,
    pub status: Option<Status>,
    pub merge_state: Option<String>,
    pub queue: Option<(i64, String)>,
    pub fail: usize,
    pub url: String,
}

/// Build rows sorted by PR number descending.
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
            }
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.number));
    rows
}

/// Style for a `mergeStateStatus` value, using the shared palette.
pub fn state_style(state: &str) -> Style {
    match state {
        "CLEAN" | "HAS_HOOKS" => status::fg(GREEN),
        "BLOCKED" | "BEHIND" => status::fg(YELLOW),
        "DIRTY" | "DRAFT" => status::fg(RED),
        _ => Style::new().dimmed(),
    }
}

pub fn to_table(rows: &[PrRow], ascii: bool) -> Table {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let st = match r.status {
            Some(s) => Cell::styled(status::glyph(s, ascii).to_string(), status::fg(status::status_style(s).1)),
            None => Cell::styled("-".to_string(), Style::new().dimmed()),
        };
        let pr = if r.is_draft {
            Cell::styled("draft".to_string(), Style::new().dimmed())
        } else {
            Cell::styled(format!("#{}", r.number), status::fg(BLUE))
        };
        let state_text = r.merge_state.clone().unwrap_or_else(|| "?".to_string());
        let state = Cell::styled(state_text.clone(), state_style(&state_text));
        let queue_text = match &r.queue {
            Some((pos, state)) => format!("#{pos} {state}"),
            None => "-".to_string(),
        };
        let queue_style = if queue_text.starts_with('#') {
            status::fg(YELLOW).bold()
        } else {
            Style::new().dimmed()
        };
        let (fail_text, fail_style) = if r.fail == 0 {
            ("-".to_string(), Style::new().dimmed())
        } else {
            (r.fail.to_string(), status::fg(RED).bold())
        };
        out.push(vec![
            st,
            pr,
            Cell::plain(r.title.clone()),
            state,
            Cell::styled(queue_text, queue_style),
            Cell::styled(fail_text, fail_style),
            Cell::link(r.url.clone(), r.url.clone()),
        ]);
    }
    Table {
        header: vec!["ST", "PR", "TITLE", "STATE", "QUEUE", "FAIL", "URL"],
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
            merge_queue_entry: None,
            commits: Commits {
                nodes: vec![CommitNode {
                    commit: Commit {
                        check_suites: crate::model::CheckSuites {
                            nodes: concls
                                .iter()
                                .map(|c| CheckSuite {
                                    conclusion: c.map(str::to_string),
                                    check_runs: crate::model::CheckRuns { total_count: 1 },
                                })
                                .collect(),
                        },
                    },
                }],
            },
        }
    }

    #[test]
    fn sorts_by_number_desc_and_derives_status_and_fail() {
        let rows = build_rows(vec![
            pr(10, "MERGEABLE", "BLOCKED", &[Some("SUCCESS")]),
            pr(42, "CONFLICTING", "DIRTY", &[Some("FAILURE"), Some("FAILURE")]),
        ]);
        assert_eq!(rows[0].number, 42);
        assert_eq!(rows[0].status, Some(Status::Conflicts));
        assert_eq!(rows[0].fail, 2);
        assert_eq!(rows[1].number, 10);
        assert_eq!(rows[1].status, Some(Status::Pass));
        assert_eq!(rows[1].fail, 0);
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
    fn state_styles_match_palette() {
        assert_eq!(state_style("CLEAN"), status::fg(GREEN));
        assert_eq!(state_style("BLOCKED"), status::fg(YELLOW));
        assert_eq!(state_style("DIRTY"), status::fg(RED));
        assert_eq!(state_style("WHATEVER"), Style::new().dimmed());
    }
}

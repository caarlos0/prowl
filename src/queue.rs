//! Merge-queue view: rows, sorting, styling, and table building.

use crate::model::QueueEntryNode;
use crate::render::{self, Cell, Table};
use crate::status::{self, BLUE, YELLOW};
use crate::timefmt;
use anstyle::Style;

/// Queue author logins are truncated to this many display columns.
const AUTHOR_WIDTH: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueueRow {
    pub position: i64,
    pub number: i64,
    pub author: String,
    pub title: String,
    pub url: String,
    pub mine: bool,
    /// When the entry joined the queue (WAIT = now - this).
    pub enqueued_at: Option<String>,
    /// When the speculative merge commit started building (BUILD = now - this);
    /// `None` when the entry isn't building yet.
    pub build_started_at: Option<String>,
}

/// Build rows ordered by queue position ascending; `mine` flags own PRs.
pub fn build_rows(nodes: Vec<QueueEntryNode>, me: &str) -> Vec<QueueRow> {
    let mut rows: Vec<QueueRow> = nodes
        .into_iter()
        .map(|n| {
            let build_started_at = n.build_started_at();
            let author = n
                .pull_request
                .author
                .map(|a| a.login)
                .unwrap_or_else(|| "ghost".to_string());
            QueueRow {
                position: n.position,
                number: n.pull_request.number,
                mine: author == me,
                author,
                title: n.pull_request.title,
                url: n.pull_request.url,
                enqueued_at: n.enqueued_at,
                build_started_at,
            }
        })
        .collect();
    rows.sort_by_key(|r| r.position);
    rows
}

pub fn to_table(rows: &[QueueRow], ascii: bool) -> Table {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let (meta, pr, author_style, title) = if r.mine {
            let hi = status::fg(YELLOW).bold();
            (hi, hi, hi, Style::new().bold())
        } else {
            (
                Style::new().dimmed(),
                status::fg(BLUE),
                Style::new(),
                Style::new(),
            )
        };
        let author = render::truncate(&r.author, AUTHOR_WIDTH, ascii);
        let wait = timefmt::age_of(r.enqueued_at.as_deref());
        // No check has started running yet (queued, or no speculative commit).
        let build = match r.build_started_at.as_deref() {
            Some(ts) => timefmt::age_of(Some(ts)),
            None => "\u{2014}".to_string(),
        };
        out.push(vec![
            Cell::plain(" "),
            Cell::styled(r.position.to_string(), meta),
            Cell::link_styled(format!("#{}", r.number), r.url.clone(), pr),
            Cell::styled(r.title.clone(), title),
            Cell::styled(author, author_style),
            Cell::styled(wait, meta),
            Cell::styled(build, meta),
        ]);
    }
    Table {
        // A leading (always-blank) marker column keeps the queue aligned with
        // the Open PRs and Merged PRs tables, which lead with the change marker.
        header: vec!["", "#", "PR", "TITLE", "AUTHOR", "WAIT", "BUILD"],
        rows: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Login, QueueCommit, QueueContext, QueueContexts, QueuePr, QueueRollup};

    fn node(position: i64, number: i64, login: &str) -> QueueEntryNode {
        QueueEntryNode {
            position,
            enqueued_at: None,
            head_commit: None,
            pull_request: QueuePr {
                number,
                title: format!("PR {number}"),
                url: format!("https://x/{number}"),
                author: Some(Login {
                    login: login.to_string(),
                }),
            },
        }
    }

    /// A speculative merge commit whose rollup checks started at the given times
    /// (`None` = a context with no start, e.g. a legacy status or a queued run).
    fn commit(starts: &[Option<&str>]) -> QueueCommit {
        QueueCommit {
            status_check_rollup: Some(QueueRollup {
                contexts: QueueContexts {
                    nodes: starts
                        .iter()
                        .map(|s| QueueContext {
                            started_at: s.map(str::to_string),
                        })
                        .collect(),
                },
            }),
        }
    }

    #[test]
    fn orders_by_position_and_flags_mine() {
        let rows = build_rows(
            vec![node(2, 20, "alice"), node(1, 10, "caarlos0")],
            "caarlos0",
        );
        assert_eq!(rows[0].position, 1);
        assert!(rows[0].mine);
        assert_eq!(rows[1].position, 2);
        assert!(!rows[1].mine);
    }

    #[test]
    fn missing_author_becomes_ghost() {
        let mut n = node(1, 1, "x");
        n.pull_request.author = None;
        let rows = build_rows(vec![n], "caarlos0");
        assert_eq!(rows[0].author, "ghost");
        assert!(!rows[0].mine);
    }

    #[test]
    fn build_time_is_earliest_check_run_start() {
        let mut n = node(1, 1, "caarlos0");
        n.enqueued_at = Some("2026-06-19T11:50:00Z".to_string());
        // Checks started well after the entry was enqueued; BUILD tracks the
        // earliest run start, not the enqueue time.
        n.head_commit = Some(commit(&[
            Some("2026-06-19T12:05:00Z"),
            Some("2026-06-19T12:00:00Z"),
            Some("2026-06-19T12:10:00Z"),
        ]));
        let rows = build_rows(vec![n], "caarlos0");
        assert_eq!(rows[0].enqueued_at.as_deref(), Some("2026-06-19T11:50:00Z"));
        assert_eq!(
            rows[0].build_started_at.as_deref(),
            Some("2026-06-19T12:00:00Z")
        );
    }

    #[test]
    fn build_time_empty_until_a_check_starts() {
        // No speculative commit yet -> no build time.
        let rows = build_rows(vec![node(1, 1, "caarlos0")], "caarlos0");
        assert!(rows[0].build_started_at.is_none());

        // A commit whose checks are all still queued (no `startedAt`) also has
        // no build time...
        let mut queued = node(2, 2, "caarlos0");
        queued.head_commit = Some(commit(&[None, None]));
        let rows = build_rows(vec![queued], "caarlos0");
        assert!(rows[0].build_started_at.is_none());

        // ...and both render as a dash in the BUILD column.
        let out = render::render_table(&to_table(&rows, true), false);
        assert!(out.contains("WAIT"));
        assert!(out.contains("BUILD"));
        assert!(out.contains('\u{2014}'));
    }
}

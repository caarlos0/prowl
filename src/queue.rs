//! Merge-queue view: rows, sorting, styling, and table building.

use crate::model::QueueEntryNode;
use crate::render::{Cell, Table};
use crate::status::{self, BLUE, YELLOW};
use anstyle::Style;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueRow {
    pub position: i64,
    pub number: i64,
    pub author: String,
    pub title: String,
    pub url: String,
    pub mine: bool,
}

/// Build rows ordered by queue position ascending; `mine` flags own PRs.
pub fn build_rows(nodes: Vec<QueueEntryNode>, me: &str) -> Vec<QueueRow> {
    let mut rows: Vec<QueueRow> = nodes
        .into_iter()
        .map(|n| {
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
            }
        })
        .collect();
    rows.sort_by_key(|r| r.position);
    rows
}

pub fn to_table(rows: &[QueueRow]) -> Table {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let (pos, pr, author, title) = if r.mine {
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
        out.push(vec![
            Cell::styled(r.position.to_string(), pos),
            Cell::styled(format!("#{}", r.number), pr),
            Cell::styled(r.author.clone(), author),
            Cell::styled(r.title.clone(), title),
            Cell::link(r.url.clone(), r.url.clone()),
        ]);
    }
    Table {
        header: vec!["#", "PR", "AUTHOR", "TITLE", "URL"],
        rows: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Login, QueuePr};

    fn node(position: i64, number: i64, login: &str) -> QueueEntryNode {
        QueueEntryNode {
            position,
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
}

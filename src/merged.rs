//! Recently-merged view: rows, sorting, styling, and table building. Every row
//! leads with the shared `merged` palette glyph (branch, mauve).

use crate::model::MergedNode;
use crate::render::{self, Cell, Table};
use crate::status::{self, BLUE, MAUVE, Status};
use crate::timefmt;
use anstyle::Style;
use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MergedRow {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub base: String,
    pub merged_at: Option<String>,
    pub updated_at: Option<String>,
}

/// Build rows sorted by last update time (most recent first), capped at `limit`.
pub fn build_rows(nodes: Vec<MergedNode>, limit: usize) -> Vec<MergedRow> {
    let mut rows: Vec<MergedRow> = nodes
        .into_iter()
        .map(|n| MergedRow {
            number: n.number,
            title: n.title,
            url: n.url,
            base: n.base_ref_name.unwrap_or_default(),
            merged_at: n.merged_at,
            updated_at: n.updated_at,
        })
        .collect();
    // RFC 3339 timestamps in a fixed `...Z` form sort lexically == chronologically.
    rows.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.number.cmp(&a.number))
    });
    rows.truncate(limit);
    rows
}

pub fn to_table(rows: &[MergedRow], ascii: bool, highlight: &HashSet<i64>) -> Table {
    let glyph = status::glyph(Status::Merged, ascii);
    let dim = Style::new().dimmed();
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(vec![
            render::change_marker(highlight.contains(&r.number), ascii),
            Cell::styled(glyph.to_string(), status::fg(MAUVE)),
            Cell::link_styled(format!("#{}", r.number), r.url.clone(), status::fg(BLUE)),
            Cell::plain(r.title.clone()),
            Cell::styled(r.base.clone(), dim),
            Cell::styled(timefmt::age_of(r.merged_at.as_deref()), dim),
        ]);
    }
    Table {
        header: vec!["", "", "PR", "TITLE", "BASE", "MERGED"],
        rows: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(number: i64, updated_at: &str) -> MergedNode {
        MergedNode {
            number,
            title: format!("PR {number}"),
            url: format!("https://x/{number}"),
            merged_at: Some(updated_at.to_string()),
            updated_at: Some(updated_at.to_string()),
            base_ref_name: Some("main".to_string()),
        }
    }

    #[test]
    fn sorts_by_updated_at_desc_and_caps() {
        let rows = build_rows(
            vec![
                node(1, "2026-06-10T00:00:00Z"),
                node(2, "2026-06-18T00:00:00Z"),
                node(3, "2026-06-14T00:00:00Z"),
            ],
            2,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].number, 2);
        assert_eq!(rows[1].number, 3);
    }
}

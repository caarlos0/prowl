//! Recently-merged view: rows, sorting, styling, and table building. Every row
//! leads with the shared `merged` palette glyph (branch, mauve).

use crate::commits::{ReleaseMap, ReleaseRef};
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
    /// The release that shipped this PR, or `None` when it hasn't shipped yet.
    pub release: Option<ReleaseRef>,
    pub merged_at: Option<String>,
    pub updated_at: Option<String>,
}

/// Build rows sorted by last update time (most recent first), capped at `limit`.
/// `releases` maps a PR number to the release that shipped it.
pub fn build_rows(nodes: Vec<MergedNode>, limit: usize, releases: &ReleaseMap) -> Vec<MergedRow> {
    let mut rows: Vec<MergedRow> = nodes
        .into_iter()
        .map(|n| MergedRow {
            number: n.number,
            title: n.title,
            url: n.url,
            release: releases.get(&n.number).cloned(),
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
        // The release tag links to its release page; an unshipped PR shows a dash.
        let release = match &r.release {
            Some(rr) => Cell::link(rr.tag.clone(), rr.url.clone()),
            None => Cell::styled("\u{2014}".to_string(), dim),
        };
        out.push(vec![
            render::change_marker(highlight.contains(&r.number), ascii),
            Cell::styled(glyph.to_string(), status::fg(MAUVE)),
            Cell::link_styled(format!("#{}", r.number), r.url.clone(), status::fg(BLUE)),
            Cell::plain(r.title.clone()),
            release,
            Cell::styled(timefmt::age_of(r.merged_at.as_deref()), dim),
        ]);
    }
    Table {
        header: vec!["", "", "PR", "TITLE", "RELEASE", "MERGED"],
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
            &ReleaseMap::new(),
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].number, 2);
        assert_eq!(rows[1].number, 3);
    }

    #[test]
    fn annotates_release_from_map() {
        let mut releases = ReleaseMap::new();
        releases.insert(
            2,
            ReleaseRef {
                tag: "v1.2.0".to_string(),
                url: "https://x/releases/tag/v1.2.0".to_string(),
            },
        );
        let rows = build_rows(
            vec![
                node(1, "2026-06-10T00:00:00Z"),
                node(2, "2026-06-18T00:00:00Z"),
            ],
            10,
            &releases,
        );
        // #2 shipped in v1.2.0; #1 hasn't shipped yet.
        assert_eq!(rows[0].number, 2);
        assert_eq!(rows[0].release.as_ref().unwrap().tag, "v1.2.0");
        assert!(rows[1].release.is_none());
    }
}

//! Recently-merged view: rows, sorting, styling, and table building. Every row
//! in the section is merged by definition, so it carries no per-row glyph.

use crate::commits::{ReleaseMap, ReleaseRef};
use crate::model::MergedNode;
use crate::render::{self, Cell, Table};
use crate::status::{self, BLUE};
use crate::timefmt;
use std::collections::HashSet;
use uncurses::style::Style;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MergedRow {
    pub number: i64,
    pub title: String,
    #[serde(default)]
    pub branch: String,
    pub url: String,
    /// The release that shipped this PR, if known.
    pub release: Option<ReleaseRef>,
    #[serde(default)]
    pub release_unknown: bool,
    pub merged_at: Option<String>,
}

/// Build rows sorted by merge time (most recent first), capped at `limit`.
/// `releases` maps a PR number to the release that shipped it.
pub fn build_rows(nodes: Vec<MergedNode>, limit: usize, releases: &ReleaseMap) -> Vec<MergedRow> {
    let mut rows: Vec<MergedRow> = nodes
        .into_iter()
        .map(|n| {
            let release = releases.get(&n.number).cloned();
            MergedRow {
                number: n.number,
                title: n.title,
                branch: n.head_ref_name.unwrap_or_default(),
                url: n.url,
                release_unknown: release.is_none() && !releases.is_complete(),
                release,
                merged_at: n.merged_at,
            }
        })
        .collect();
    // RFC 3339 timestamps in a fixed `...Z` form sort lexically == chronologically.
    rows.sort_by(|a, b| {
        b.merged_at
            .cmp(&a.merged_at)
            .then_with(|| b.number.cmp(&a.number))
    });
    rows.truncate(limit);
    rows
}

pub fn to_table(
    rows: &[MergedRow],
    ascii: bool,
    highlight: &HashSet<i64>,
    show_branch: bool,
) -> Table {
    let dim = Style::new().faint();
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        // An unknown release lookup must not look like a confirmed unshipped PR.
        let release = match &r.release {
            Some(rr) => Cell::link(rr.tag.clone(), rr.url.clone()),
            None if r.release_unknown => Cell::styled("?".to_string(), &dim),
            None => Cell::styled("\u{2014}".to_string(), &dim),
        };
        let mut row = vec![
            render::change_marker(highlight.contains(&r.number), ascii),
            // A blank glyph cell: every row here is merged, so there is no
            // approval to report, but the PR column has to start where the
            // open-PRs table starts.
            Cell::plain(" "),
            Cell::pr(r.number, r.url.clone(), status::fg(BLUE)),
            Cell::plain(r.title.clone()),
        ];
        if show_branch {
            row.push(Cell::styled(r.branch.clone(), &dim));
        }
        row.extend([
            release,
            Cell::styled(timefmt::age_of(r.merged_at.as_deref()), &dim),
        ]);
        out.push(row);
    }
    let mut header = vec!["", "", "PR", "TITLE"];
    if show_branch {
        header.push("BRANCH");
    }
    header.extend(["RELEASE", "MERGED"]);
    Table { header, rows: out }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(number: i64, merged_at: &str) -> MergedNode {
        MergedNode {
            number,
            title: format!("PR {number}"),
            url: format!("https://x/{number}"),
            head_ref_name: Some(format!("branch-{number}")),
            author: None,
            merged_at: Some(merged_at.to_string()),
        }
    }

    #[test]
    fn sorts_by_merged_at_desc_and_caps() {
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
        assert_eq!(rows[0].branch, "branch-2");
        assert_eq!(rows[1].number, 3);
        assert!(
            !to_table(&rows, true, &HashSet::new(), false)
                .header
                .contains(&"BRANCH")
        );
        assert!(
            to_table(&rows, true, &HashSet::new(), true)
                .header
                .contains(&"BRANCH")
        );
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

    #[test]
    fn unknown_release_is_not_rendered_as_unshipped() {
        let rows = build_rows(
            vec![node(1, "2026-06-10T00:00:00Z")],
            10,
            &ReleaseMap::incomplete(),
        );
        assert!(rows[0].release_unknown);
        assert_eq!(
            to_table(&rows, true, &HashSet::new(), false).rows[0][4].text,
            "?"
        );
    }
}

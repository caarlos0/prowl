//! Offline, fixture-based tests: real (and a crafted) GitHub GraphQL API
//! responses are parsed through the same path the binary uses, then turned into
//! rows and rendered. No network access.

use prowl::model::{MergedData, MineData, QueueData};
use prowl::status::Status;
use prowl::{github, merged, prs, queue, render};
use std::collections::HashSet;

fn parse<T: serde::de::DeserializeOwned>(json: &str) -> T {
    github::parse_graphql(json.as_bytes()).expect("fixture should parse")
}

// ---------------------------------------------------------------------------
// Merge queue
// ---------------------------------------------------------------------------

#[test]
fn queue_parses_orders_and_flags_mine() {
    let data: QueueData = parse(include_str!("fixtures/queue_populated.json"));
    let rows = queue::build_rows(model_queue_nodes(data), "caarlos0");

    // Input positions are 2,1,3; rows come out ordered by position ascending.
    assert_eq!(
        rows.iter().map(|r| r.position).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    // caarlos0 is at position 1 -> mine; octocat is not.
    assert!(rows[0].mine);
    assert_eq!(rows[0].number, 101);
    assert!(!rows[1].mine);
    // A null author renders as "ghost" and is never mine.
    assert_eq!(rows[2].author, "ghost");
    assert!(!rows[2].mine);
}

#[test]
fn queue_null_and_empty_both_yield_no_rows() {
    let null: QueueData = parse(include_str!("fixtures/queue_null.json"));
    let empty: QueueData = parse(include_str!("fixtures/queue_empty.json"));
    assert!(model_queue_nodes(null).is_empty());
    assert!(model_queue_nodes(empty).is_empty());
}

#[test]
fn queue_styled_render_uses_palette_and_links() {
    let data: QueueData = parse(include_str!("fixtures/queue_populated.json"));
    let rows = queue::build_rows(model_queue_nodes(data), "caarlos0");
    let out = render::render_table(&queue::to_table(&rows, false), true);

    // Mine row is highlighted yellow (#f9e2af); others' PR cell is blue (#89b4fa).
    assert!(out.contains("38;2;249;226;175"), "expected mine yellow");
    assert!(out.contains("38;2;137;180;250"), "expected not-mine blue");
    // URLs are OSC-8 hyperlinks.
    assert!(out.contains("\x1b]8;;https://github.com/octo/repo/pull/101\x1b\\"));
}

// ---------------------------------------------------------------------------
// My open PRs
// ---------------------------------------------------------------------------

#[test]
fn mine_parses_sorts_and_derives_status_and_fail() {
    let data: MineData = parse(include_str!("fixtures/mine.json"));
    let rows = prs::build_rows(data.search.nodes);

    // Sorted by last update time (most recent first).
    assert_eq!(
        rows.iter().map(|r| r.number).collect::<Vec<_>>(),
        vec![6656, 6475, 5323]
    );
    let upd: Vec<&Option<String>> = rows.iter().map(|r| &r.updated_at).collect();
    assert!(upd.windows(2).all(|w| w[0] >= w[1]), "updatedAt descending");
    // #6656 is mergeable; its only non-success suite is a zero-run phantom, so
    // it is green (pass) with no failures — not pending.
    assert_eq!(rows[0].status, Some(Status::Pass));
    assert_eq!(rows[0].fail, 0);
    // #6475 conflicts (which beats its failing checks), but the FAIL count is
    // still the real number of failing suites that actually ran.
    assert_eq!(rows[1].status, Some(Status::Conflicts));
    assert_eq!(rows[1].fail, 3);
    // #5323 conflicts with no check suites -> no fails.
    assert_eq!(rows[2].status, Some(Status::Conflicts));
    assert_eq!(rows[2].fail, 0);
}

#[test]
fn mine_ascii_status_letters() {
    let data: MineData = parse(include_str!("fixtures/mine.json"));
    let rows = prs::build_rows(data.search.nodes);
    let table = prs::to_table(&rows, true, &HashSet::new()); // ascii, no highlights
    // Column 0 is the change marker; column 1 is the status glyph.
    let st: Vec<&str> = table.rows.iter().map(|r| r[1].text.as_str()).collect();
    assert_eq!(st, vec!["P", "!", "!"]); // pass, conflicts, conflicts
}

#[test]
fn mine_changed_rows_get_a_marker() {
    let data: MineData = parse(include_str!("fixtures/mine.json"));
    let rows = prs::build_rows(data.search.nodes);
    let highlight = HashSet::from([6475]);
    let table = prs::to_table(&rows, true, &highlight);
    let marks: Vec<&str> = table.rows.iter().map(|r| r[0].text.as_str()).collect();
    assert_eq!(marks, vec![" ", ">", " "]); // only #6475 is flagged
}

#[test]
fn mine_empty_yields_no_rows() {
    let data: MineData = parse(include_str!("fixtures/mine_empty.json"));
    assert!(data.search.nodes.is_empty());
}

#[test]
fn mine_tolerates_null_check_runs_and_partial_errors() {
    // GitHub returns a null `checkRuns` for a suite the viewer can't see, and
    // attaches a top-level `errors` array. The response must still parse (the
    // suite deserializes as `None`) and that suite must be ignored as a phantom
    // rather than failing the whole fetch.
    let data: MineData = parse(include_str!("fixtures/mine_null_checkruns.json"));
    let rows = prs::build_rows(data.search.nodes);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].number, 123);
    // The accessible suite passed; the inaccessible (null) one is ignored.
    assert_eq!(rows[0].status, Some(Status::Pass));
    assert_eq!(rows[0].fail, 0);
}

#[test]
fn mine_partial_null_surfaces_graphql_error() {
    // `data` is present but a required (non-Option) field — `checkSuites` — is
    // null, so typing the data fails. With an `errors` array attached, the real
    // GitHub message must surface instead of a generic JSON parse error.
    let err = github::parse_graphql::<MineData>(
        include_str!("fixtures/mine_null_checksuites.json").as_bytes(),
    )
    .expect_err("a null non-Option field should fail to type");
    assert!(
        err.to_string()
            .contains("Resource not accessible by integration"),
        "expected the GraphQL error to surface, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Recently merged
// ---------------------------------------------------------------------------

#[test]
fn merged_parses_sorts_desc_and_caps() {
    let data: MergedData = parse(include_str!("fixtures/merged.json"));
    let rows = merged::build_rows(data.search.nodes, 4);

    assert_eq!(rows.len(), 4); // capped at the limit
    // Most recently updated first.
    assert_eq!(rows[0].number, 6649);
    assert_eq!(rows[0].base, "main");
    // Strictly descending update timestamps.
    let ts: Vec<&Option<String>> = rows.iter().map(|r| &r.updated_at).collect();
    assert!(ts.windows(2).all(|w| w[0] >= w[1]));
}

#[test]
fn merged_empty_yields_no_rows() {
    let data: MergedData = parse(include_str!("fixtures/merged_empty.json"));
    assert!(data.search.nodes.is_empty());
}

// `model::queue_nodes` takes ownership; a tiny shim keeps the call sites tidy.
fn model_queue_nodes(data: QueueData) -> Vec<prowl::model::QueueEntryNode> {
    prowl::model::queue_nodes(data)
}

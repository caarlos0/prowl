//! Offline, fixture-based tests: real (and a crafted) `gh api graphql`
//! responses are parsed through the same path the binary uses, then turned into
//! rows and rendered. No network access.

use prowl::model::{MergedData, MineData, QueueData};
use prowl::status::Status;
use prowl::{gh, merged, prs, queue, render};

fn parse<T: serde::de::DeserializeOwned>(json: &str) -> T {
    gh::parse_graphql(json.as_bytes()).expect("fixture should parse")
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
    let out = render::render_table(&queue::to_table(&rows), true);

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

    // Sorted by number descending.
    assert_eq!(
        rows.iter().map(|r| r.number).collect::<Vec<_>>(),
        vec![6656, 6475, 5323]
    );
    // #6656 is mergeable+blocked with an in-flight check -> pending, no failures.
    assert_eq!(rows[0].status, Some(Status::Pending));
    assert_eq!(rows[0].fail, 0);
    // #6475 conflicts (which beats its failing checks), but the FAIL count is
    // still the real number of failing suites.
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
    let table = prs::to_table(&rows, true); // ascii = true
    let st: Vec<&str> = table.rows.iter().map(|r| r[0].text.as_str()).collect();
    assert_eq!(st, vec![".", "!", "!"]); // pending, conflicts, conflicts
}

#[test]
fn mine_empty_yields_no_rows() {
    let data: MineData = parse(include_str!("fixtures/mine_empty.json"));
    assert!(data.search.nodes.is_empty());
}

// ---------------------------------------------------------------------------
// Recently merged
// ---------------------------------------------------------------------------

#[test]
fn merged_parses_sorts_desc_and_caps() {
    let data: MergedData = parse(include_str!("fixtures/merged.json"));
    let rows = merged::build_rows(data.search.nodes, 4);

    assert_eq!(rows.len(), 4); // capped at the limit
    // Most recent merge first.
    assert_eq!(rows[0].number, 6649);
    assert_eq!(rows[0].base, "main");
    // Strictly descending merge timestamps.
    let ts: Vec<&Option<String>> = rows.iter().map(|r| &r.merged_at).collect();
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

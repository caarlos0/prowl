//! prowl — watch a repo's open PRs, merge queue, and recently merged PRs.
//!
//! The crate is split into a small library (this file plus its modules) and a
//! thin binary so the parsing/rendering/change-detection logic can be exercised
//! by offline, fixture-based tests under `tests/`.

#![warn(clippy::pedantic)]
// Pedantic lints that are noise for this small binary crate. Its `pub` items
// exist so the offline fixture tests can reach them, not as a stable public API,
// so most "document/annotate the public surface" lints don't apply.
#![allow(clippy::must_use_candidate)] // internal API; blanket #[must_use] is noise
#![allow(clippy::return_self_not_must_use)] // same, for builder-style methods
#![allow(clippy::missing_errors_doc)] // anyhow Results; the failure modes are self-evident
#![allow(clippy::missing_panics_doc)] // the only panics are non-poisonable mutex locks
#![allow(clippy::struct_excessive_bools)] // clap flag structs are naturally bool-heavy
#![allow(clippy::struct_field_names)] // serde structs mirror GitHub's JSON field names
#![allow(clippy::implicit_hasher)] // internal HashSet params use the one default hasher
#![allow(clippy::needless_pass_by_value)] // by-value serde_json::Value is the ergonomic form
#![allow(clippy::borrow_as_ptr)] // FFI-boundary `&x` coercions read clearer than ptr::from_ref
#![allow(clippy::needless_raw_string_hashes)] // `r#"…"#` is the convention for query blocks
#![allow(clippy::format_push_string)]
// The few numeric casts are bounded/guarded (poll timeout, non-negative display
// seconds); the one size-sensitive calc — the duration parser — uses checked_mul.
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::duration_suboptimal_units)] // tests spell durations in seconds on purpose

pub mod auth;
pub mod cache;
pub mod changes;
pub mod cli;
pub mod commits;
pub mod github;
pub mod merged;
pub mod model;
pub mod nav;
pub mod open;
pub mod prs;
pub mod queue;
pub mod render;
pub mod reviews;
pub mod status;
pub mod term;
pub mod timefmt;

use anyhow::{Context, Result};
use changes::{Changes, Tracker};
use clap::Parser;
use cli::{Cli, View};
use github::{Client, Repo};
use std::io::{IsTerminal, Write};
use unicode_width::UnicodeWidthStr;

/// A fetched snapshot of every enabled section (`None` = section disabled).
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Sections {
    merged: Option<Vec<merged::MergedRow>>,
    queue: Option<Vec<queue::QueueRow>>,
    /// Queue-level estimate: seconds until a newly added entry would merge.
    queue_next_eta: Option<i64>,
    prs: Option<Vec<prs::PrRow>>,
    commits: Option<commits::CommitStats>,
    /// Reviews view: open PRs awaiting / under my review.
    reviews: Option<Vec<reviews::ReviewRow>>,
    /// Reviews view: merged PRs I reviewed.
    reviewed_merged: Option<Vec<reviews::ReviewedMergedRow>>,
}

/// Fetch the sections for the requested views. `want_mine` covers the Mine view
/// (open PRs, queue, merged, shipments, honoring `--only`); `want_reviews`
/// covers the Reviews view (PRs to review, reviewed-and-merged). In watch mode
/// both are fetched so Tab can switch instantly; `--once` fetches just one.
fn fetch(
    cli: &Cli,
    client: &Client,
    repo: &Repo,
    me: &str,
    default_branch: &str,
    want_mine: bool,
    want_reviews: bool,
) -> Result<Sections> {
    // Release data powers both the "My Shipments" counts and the merged
    // "RELEASE" column, so fetch it once when either section is shown.
    // Best-effort: a failure (no releases, empty repo, ...) degrades to an
    // "unavailable" shipments line and blank release cells rather than taking
    // down the whole dashboard.
    let (commit_stats, release_map) = if want_mine && (cli.show_shipments() || cli.show_merged()) {
        commits::fetch(client, repo, me, default_branch, cli.include_pre_releases).ok()
    } else {
        None
    }
    .unwrap_or_else(|| {
        (
            commits::CommitStats::unavailable(),
            commits::ReleaseMap::new(),
        )
    });

    let merged = if want_mine && cli.show_merged() {
        let since = timefmt::since_date(&cli.merged_window);
        let nodes = model::fetch_merged(client, repo, me, &since, cli.merged_limit)?;
        Some(merged::build_rows(nodes, cli.merged_limit, &release_map))
    } else {
        None
    };
    let (queue, queue_next_eta) = if want_mine && cli.show_queue() {
        let (nodes, eta) = model::fetch_queue(client, repo)?;
        (Some(queue::build_rows(nodes, me)), eta)
    } else {
        (None, None)
    };
    let prs = if want_mine && cli.show_mine() {
        Some(prs::build_rows(model::fetch_my_prs(client, repo, me)?))
    } else {
        None
    };
    let commits = (want_mine && cli.show_shipments()).then_some(commit_stats);

    // Reviews view: PRs awaiting / under my review, plus merged PRs I reviewed.
    let (reviews, reviewed_merged) = if want_reviews {
        let data = model::fetch_reviews(client, repo, me, cli.review_scope.qualifier())?;
        let open = reviews::build_open_rows(data);
        let since = timefmt::since_date(&cli.merged_window);
        let merged_nodes =
            model::fetch_reviewed_merged(client, repo, me, &since, cli.merged_limit)?;
        let merged_reviews = reviews::build_merged_rows(merged_nodes, cli.merged_limit);
        (Some(open), Some(merged_reviews))
    } else {
        (None, None)
    };

    Ok(Sections {
        merged,
        queue,
        queue_next_eta,
        prs,
        commits,
        reviews,
        reviewed_merged,
    })
}

/// Synthetic dashboard data for `--demo` (screenshots): no auth, repo, or
/// network. Times are relative to now so the ages look fresh. Temporary.
#[cfg(feature = "demo")]
fn demo_sections() -> Sections {
    use chrono::{SecondsFormat, Utc};
    let ago = |secs: i64| {
        (Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339_opts(SecondsFormat::Secs, true)
    };
    let pr =
        |number, is_draft, title: &str, status, merge_state: &str, queue, fail, secs| prs::PrRow {
            number,
            is_draft,
            title: title.to_string(),
            status,
            merge_state: Some(merge_state.to_string()),
            queue,
            fail,
            url: format!("https://github.com/caarlos0/prowl/pull/{number}"),
            updated_at: Some(ago(secs)),
        };
    use status::Status::*;
    let prs = vec![
        pr(
            128,
            false,
            "feat(render): truecolor status palette",
            Some(Pass),
            "CLEAN",
            None,
            0,
            300,
        ),
        pr(
            127,
            false,
            "fix(term): restore cursor on SIGTSTP",
            Some(Fail),
            "BLOCKED",
            None,
            2,
            1080,
        ),
        pr(
            125,
            false,
            "perf(cache): paint from disk on startup",
            Some(Pending),
            "UNSTABLE",
            None,
            0,
            2400,
        ),
        pr(
            124,
            true,
            "wip: nix flake + home-manager module",
            None,
            "DRAFT",
            None,
            0,
            7200,
        ),
        pr(
            120,
            false,
            "chore(deps): bump ureq to 3.x",
            Some(Conflicts),
            "DIRTY",
            None,
            0,
            10800,
        ),
        pr(
            118,
            false,
            "feat(queue): inline merge-queue position",
            Some(Pass),
            "CLEAN",
            Some((1, "QUEUED".to_string())),
            0,
            3600,
        ),
    ];

    let qrow =
        |position, number, author: &str, title: &str, mine, wait_secs, build_secs: Option<i64>| {
            queue::QueueRow {
                position,
                number,
                author: author.to_string(),
                title: title.to_string(),
                url: format!("https://github.com/caarlos0/prowl/pull/{number}"),
                mine,
                enqueued_at: Some(ago(wait_secs)),
                build_started_at: build_secs.map(|s| ago(s)),
            }
        };
    let queue = vec![
        qrow(
            1,
            118,
            "caarlos0",
            "feat(queue): inline merge-queue position",
            true,
            720,
            Some(480),
        ),
        qrow(
            2,
            131,
            "dependabot[bot]",
            "build(deps): bump anstyle to 1.1",
            false,
            480,
            Some(180),
        ),
        qrow(
            3,
            117,
            "octocat",
            "docs: clarify the --only flag",
            false,
            300,
            None,
        ),
    ];

    let base = "https://github.com/caarlos0/prowl";
    let rel = |tag: &str| {
        Some(commits::ReleaseRef {
            tag: tag.to_string(),
            url: format!("{base}/releases/tag/{tag}"),
        })
    };
    let mrow = |number, title: &str, secs, release| merged::MergedRow {
        number,
        title: title.to_string(),
        url: format!("{base}/pull/{number}"),
        release,
        merged_at: Some(ago(secs)),
    };
    // Recent merges aren't shipped yet (None); older ones map to a release.
    let merged = vec![
        mrow(119, "feat(status): ignore phantom check suites", 720, None),
        mrow(116, "fix(github): exact-match the remote host", 7200, None),
        mrow(
            112,
            "ci: build a snapshot on pull requests",
            86_400,
            rel("v0.4.0"),
        ),
        mrow(
            108,
            "feat(render): OSC-8 hyperlinks for URLs",
            259_200,
            rel("v0.3.0"),
        ),
    ];

    let bucket = |mine, capped, url: String| commits::Bucket {
        count: commits::Count { mine, capped },
        url,
    };
    let release = |tag: &str, mine, capped, secs| commits::Release {
        tag: tag.to_string(),
        bucket: bucket(mine, capped, format!("{base}/releases/tag/{tag}")),
        published_at: Some(ago(secs)),
    };
    let commits = commits::CommitStats {
        available: true,
        upcoming: Some(bucket(7, false, format!("{base}/compare/v0.4.0...main"))),
        releases: vec![
            release("v0.4.0", 12, false, 432_000),
            release("v0.3.0", 9, false, 1_728_000),
            release("v0.2.0", 31, true, 3_456_000),
            release("v0.1.0", 18, false, 6_048_000),
        ],
    };

    // Reviews-view demo data (shown with `--demo --view reviews`).
    use status::ReviewState;
    let rrow = |number, author: &str, title: &str, state, secs| reviews::ReviewRow {
        number,
        is_draft: false,
        title: title.to_string(),
        author: author.to_string(),
        url: format!("{base}/pull/{number}"),
        state,
        updated_at: Some(ago(secs)),
    };
    let reviews = vec![
        rrow(
            142,
            "octocat",
            "feat(api): paginate the search endpoint",
            ReviewState::Awaiting,
            420,
        ),
        rrow(
            139,
            "hubot",
            "fix(auth): refresh tokens before expiry",
            ReviewState::ReReview,
            1500,
        ),
        rrow(
            133,
            "dependabot[bot]",
            "build(deps): bump rustls to 0.24",
            ReviewState::Updated,
            5400,
        ),
        rrow(
            130,
            "octocat",
            "docs: expand the troubleshooting guide",
            ReviewState::Reviewed,
            9000,
        ),
    ];
    let mrow_rev = |number, author: &str, title: &str, secs| reviews::ReviewedMergedRow {
        number,
        title: title.to_string(),
        author: author.to_string(),
        url: format!("{base}/pull/{number}"),
        merged_at: Some(ago(secs)),
    };
    let reviewed_merged = vec![
        mrow_rev(
            126,
            "hubot",
            "refactor(store): drop the legacy cache path",
            64_800,
        ),
        mrow_rev(
            122,
            "octocat",
            "test(queue): cover the empty queue",
            172_800,
        ),
    ];

    Sections {
        merged: Some(merged),
        queue: Some(queue),
        queue_next_eta: Some(11 * 60),
        prs: Some(prs),
        commits: Some(commits),
        reviews: Some(reviews),
        reviewed_merged: Some(reviewed_merged),
    }
}

/// Render one PR section: a counted header, then either its table or, when
/// empty, a dim placeholder. `table` is `None` exactly when the section is
/// empty (empties are filtered out before alignment).
fn section(
    f: &mut String,
    (title, accent): (&str, status::Rgb),
    count: usize,
    note: Option<&str>,
    empty_msg: &str,
    table: Option<&render::Table>,
    styled: bool,
) {
    f.push_str(&render::header(
        title,
        accent,
        Some(&count.to_string()),
        note,
        styled,
    ));
    f.push('\n');
    if let Some(table) = table {
        f.push_str(&render::render_table(table, styled));
    } else {
        f.push_str(&render::empty_line(empty_msg, styled));
        f.push('\n');
    }
    f.push('\n');
}

/// Render the body for `view` (no screen-clear, no footer). In watch mode it
/// leads with the `my PRs / reviews` tab strip, then the active view's sections.
/// Rows that changed since the previous refresh (per `changes`) are flagged with
/// a leading marker. The footer and help legend are rendered separately (below
/// the body) by `bottom`.
fn render_body(
    s: &Sections,
    cli: &Cli,
    view: View,
    changes: &Changes,
    styled: bool,
    selected: Option<usize>,
) -> String {
    let mut f = String::new();
    // The tab strip is an interactive affordance, so only while watching (styled
    // implies a watch TTY); piped/`--once` output goes straight to the sections.
    if styled {
        f.push_str(&render::tabs(view, styled));
        f.push_str("\n\n");
    }
    match view {
        View::Mine => render_mine(&mut f, s, cli, changes, styled, selected),
        View::Reviews => render_reviews(&mut f, s, cli, styled, selected),
    }
    f
}

/// Overwrite the leading marker cell of `table`'s `local`-th row with the
/// navigation caret (a no-op when there's no such table or row).
fn place_caret(table: Option<&mut render::Table>, local: usize, ascii: bool) {
    if let Some(row) = table.and_then(|t| t.rows.get_mut(local))
        && let Some(first) = row.first_mut()
    {
        *first = render::select_marker(ascii);
    }
}

/// The interactive dashboard state the watch loop threads through rendering and
/// mutates on each keypress.
struct Ui {
    /// Active view; starts at `--view`, toggled with Tab.
    view: View,
    /// Whether the `?` help legend is shown (starts hidden).
    show_help: bool,
    /// Navigation cursor into the active view's rows — lazy (`None` until the
    /// user moves it), reset when switching views, clamped when a refresh shrinks
    /// the list.
    selected: Option<usize>,
    /// The trailing status line, empty unless a refresh or an open failed.
    last_status: String,
}

/// What the watch loop should do after a keypress.
enum Act {
    /// `r`, or the interval elapsed: (re)fetch now.
    Refresh,
    /// State changed — repaint the dashboard.
    Repaint,
    /// Nothing to do.
    Idle,
}

impl Ui {
    /// Classify a keypress into the loop's next `Act`, applying its state change
    /// (help, view switch, `o` open, or a movement). Shared by the idle wait and
    /// the mid-fetch poll so both react to keys identically.
    fn on_key(&mut self, action: term::Wait, last_good: Option<&Sections>, styled: bool) -> Act {
        match action {
            term::Wait::Tick | term::Wait::Refresh => Act::Refresh,
            term::Wait::ToggleHelp => {
                self.show_help = !self.show_help;
                Act::Repaint
            }
            term::Wait::SwitchView => {
                // Selection indices don't carry across views, so start fresh.
                self.view = self.view.toggle();
                self.selected = None;
                Act::Repaint
            }
            term::Wait::Open => {
                if self.open_selected(last_good, styled) {
                    Act::Repaint
                } else {
                    Act::Idle
                }
            }
            _ => {
                let len = last_good.map_or(0, |s| nav::targets(self.view, s).len());
                let half = term::height().map_or(10, |h| usize::from(h / 2).max(1));
                let next = nav::moved(action, self.selected, len, half);
                if next == self.selected {
                    Act::Idle
                } else {
                    self.selected = next;
                    Act::Repaint
                }
            }
        }
    }

    /// Open the selected row's URL in the browser. Returns whether the frame
    /// should be repainted — only when opening failed (a dim error line is set);
    /// a no-op (no selection / no data / success) leaves the screen as is.
    fn open_selected(&mut self, last_good: Option<&Sections>, styled: bool) -> bool {
        let Some(sel) = self.selected else {
            return false;
        };
        let Some(good) = last_good else { return false };
        let Some(url) = nav::targets(self.view, good).get(sel).copied() else {
            return false;
        };
        if let Err(e) = open::url(url) {
            self.last_status = error_trailing(&format!("open failed: {e}"), styled);
            return true;
        }
        false
    }
}

/// The Mine view: My open PRs, then Merge Queue, then My merged PRs, then My
/// Shipments. Each section always shows its header (with a count); an empty
/// section follows it with a dim placeholder one-liner.
fn render_mine(
    f: &mut String,
    s: &Sections,
    cli: &Cli,
    changes: &Changes,
    styled: bool,
    selected: Option<usize>,
) {
    let ascii = cli.ascii || !styled;

    // Build the section tables first, then cap and align their TITLE columns so
    // the three tables line up and the whole view stays within MAX_WIDTH.
    let mut prs_table = s
        .prs
        .as_ref()
        .filter(|r| !r.is_empty())
        .map(|rows| prs::to_table(rows, ascii, &changes.status_changed));
    let mut queue_table = s
        .queue
        .as_ref()
        .filter(|r| !r.is_empty())
        .map(|rows| queue::to_table(rows, ascii));
    let mut merged_table = s
        .merged
        .as_ref()
        .filter(|r| !r.is_empty())
        .map(|rows| merged::to_table(rows, ascii, &changes.newly_merged));
    {
        let mut tables: Vec<&mut render::Table> = [
            prs_table.as_mut(),
            queue_table.as_mut(),
            merged_table.as_mut(),
        ]
        .into_iter()
        .flatten()
        .collect();
        render::fit_titles(&mut tables, ascii);
    }

    // Place the navigation caret: map the global selection onto the section it
    // falls in. Every PR/queue/merged row is navigable, so the local index is
    // the offset past the earlier sections; any remainder indexes the shipments'
    // navigable rows (handled by `render_commits`).
    let mut ship_sel = None;
    if let Some(sel) = selected {
        let np = s.prs.as_ref().map_or(0, Vec::len);
        let nq = s.queue.as_ref().map_or(0, Vec::len);
        let nm = s.merged.as_ref().map_or(0, Vec::len);
        if sel < np {
            place_caret(prs_table.as_mut(), sel, ascii);
        } else if sel < np + nq {
            place_caret(queue_table.as_mut(), sel - np, ascii);
        } else if sel < np + nq + nm {
            place_caret(merged_table.as_mut(), sel - np - nq, ascii);
        } else {
            ship_sel = Some(sel - np - nq - nm);
        }
    }

    if let Some(rows) = &s.prs {
        section(
            f,
            ("My open PRs", status::LAVENDER),
            rows.len(),
            None,
            "No open PRs.",
            prs_table.as_ref(),
            styled,
        );
    }

    if let Some(rows) = &s.queue {
        // The queue-level ETA (time until a newly added entry would merge) rides
        // alongside the header as a dim note.
        let eta = s.queue_next_eta.map(|secs| {
            format!(
                "~{} to merge",
                timefmt::eta(std::time::Duration::from_secs(secs.max(0) as u64))
            )
        });
        section(
            f,
            ("Merge Queue", status::BLUE),
            rows.len(),
            eta.as_deref(),
            "No merge queue.",
            queue_table.as_ref(),
            styled,
        );
    }

    if let Some(rows) = &s.merged {
        section(
            f,
            ("My merged PRs", status::MAUVE),
            rows.len(),
            None,
            "No recent merged PRs.",
            merged_table.as_ref(),
            styled,
        );
    }

    if let Some(stats) = &s.commits {
        render_commits(f, stats, ascii, ship_sel, styled);
        f.push('\n');
    }
}

/// The Reviews view: PRs to review (with a per-row review-state glyph), then
/// merged PRs I reviewed. Their TITLE columns are aligned together.
fn render_reviews(f: &mut String, s: &Sections, cli: &Cli, styled: bool, selected: Option<usize>) {
    let ascii = cli.ascii || !styled;

    let mut open_table = s
        .reviews
        .as_ref()
        .filter(|r| !r.is_empty())
        .map(|rows| reviews::open_to_table(rows, ascii));
    let mut merged_table = s
        .reviewed_merged
        .as_ref()
        .filter(|r| !r.is_empty())
        .map(|rows| reviews::merged_to_table(rows, ascii));
    {
        let mut tables: Vec<&mut render::Table> = [open_table.as_mut(), merged_table.as_mut()]
            .into_iter()
            .flatten()
            .collect();
        render::fit_titles(&mut tables, ascii);
    }

    // Place the navigation caret: the open reviews come first, then the reviewed
    // & merged rows, so the selection index past the open rows indexes the latter.
    if let Some(sel) = selected {
        let nr = s.reviews.as_ref().map_or(0, Vec::len);
        if sel < nr {
            place_caret(open_table.as_mut(), sel, ascii);
        } else {
            place_caret(merged_table.as_mut(), sel - nr, ascii);
        }
    }

    if let Some(rows) = &s.reviews {
        section(
            f,
            ("Reviews", status::LAVENDER),
            rows.len(),
            None,
            "No PRs to review.",
            open_table.as_ref(),
            styled,
        );
    }

    if let Some(rows) = &s.reviewed_merged {
        section(
            f,
            ("Reviewed & merged", status::MAUVE),
            rows.len(),
            None,
            "No reviewed PRs merged recently.",
            merged_table.as_ref(),
            styled,
        );
    }
}

/// The help legend for `view` (the glyphs that view uses), shown at the very
/// bottom. Empty (no leading/trailing blank) when `show_help` is false; ends
/// with a newline otherwise.
fn help_block(cli: &Cli, view: View, show_help: bool, styled: bool) -> String {
    if !show_help {
        return String::new();
    }
    let ascii = cli.ascii || !styled;
    render::help(view, ascii, styled)
}

/// Compose the bottom of the frame in order: an optional error line (empty
/// unless a refresh failed), then (watch only) the `r refresh (every 5m) - ?
/// help` footer, then the help legend last. Any part may be empty to omit it;
/// present parts are separated by a single blank line. The render body already
/// ends with a blank line, so the first part is not prefixed with one.
fn bottom(status: &str, footer: &str, help: &str) -> String {
    let mut out = String::new();
    for part in [status, footer, help] {
        if part.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(part);
        if !part.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Render the "My Shipments" section: my commit counts for the next
/// (unreleased) version and the last few stable releases, one labelled row
/// each with the labels right-aligned so the colons and counts line up. Each
/// label links out — the upcoming one to the compare log, each release to its
/// release page — and shipped releases also show how long ago they were
/// published, aligned into a trailing column.
fn render_commits(
    f: &mut String,
    stats: &commits::CommitStats,
    ascii: bool,
    selected: Option<usize>,
    styled: bool,
) {
    if !stats.available {
        f.push_str(&render::empty_line("Commit stats unavailable.", styled));
        f.push('\n');
        return;
    }
    let count = |c: &commits::Count| format!("{}{}", c.mine, if c.capped { "+" } else { "" });

    // Total commits by me across everything shown (upcoming + the releases); a
    // `+` if any bucket hit the compare API's window and is a lower bound.
    let (total, capped) = stats
        .upcoming
        .iter()
        .map(|b| &b.count)
        .chain(stats.releases.iter().map(|r| &r.bucket.count))
        .fold((0usize, false), |(n, capped), c| {
            (n + c.mine, capped || c.capped)
        });
    let total = format!("{total}{}", if capped { "+" } else { "" });
    f.push_str(&render::header(
        "My Shipments",
        status::TEAL,
        Some(&total),
        None,
        styled,
    ));
    f.push('\n');

    // Each row: the upcoming (unreleased) version first (no publish age), then
    // the shipped releases newest-first with their relative publish age. A row
    // with a URL renders its label as a link to it.
    let value = |b: Option<&commits::Bucket>| match b {
        Some(b) => count(&b.count),
        None => "\u{2014}".to_string(),
    };
    let mut rows: Vec<(String, Option<String>, String, Option<String>)> = vec![(
        "upcoming".to_string(),
        stats.upcoming.as_ref().map(|b| b.url.clone()),
        value(stats.upcoming.as_ref()),
        None,
    )];
    for r in &stats.releases {
        let age = r.published_at.as_deref().map(|p| timefmt::age_of(Some(p)));
        rows.push((
            r.tag.clone(),
            Some(r.bucket.url.clone()),
            value(Some(&r.bucket)),
            age,
        ));
    }

    // Right-align the labels and pad the counts to shared widths, so the
    // colons, counts, and publish ages each line up in a readable column.
    let label_w = rows.iter().map(|(l, ..)| l.width()).max().unwrap_or(0);
    let value_w = rows.iter().map(|(.., v, _)| v.width()).max().unwrap_or(0);

    // The selection index counts only navigable (URL-bearing) rows; the sole
    // url-less row is a commit-less "upcoming", which then shifts the rendered
    // caret row down by one.
    let sel_row = selected.map(|k| if stats.upcoming.is_some() { k } else { k + 1 });

    for (i, (label, url, value, age)) in rows.iter().enumerate() {
        // The first row is the upcoming (unreleased) version; set it apart in
        // italics. The label links to the bucket's log/release page.
        let style = if i == 0 {
            anstyle::Style::new().italic()
        } else {
            anstyle::Style::new()
        };
        let cell = match url {
            Some(url) => render::Cell::link_styled(label.clone(), url.clone(), style),
            None => render::Cell::styled(label.clone(), style),
        };
        // A 2-column leading gutter: the caret on the selected row, else blank,
        // keeping the labels aligned with or without a caret.
        let gutter = if Some(i) == sel_row {
            format!(
                "{} ",
                render::render_cell(&render::select_marker(ascii), styled)
            )
        } else {
            "  ".to_string()
        };
        let lpad = " ".repeat(label_w - label.width());
        f.push_str(&format!(
            "{gutter}{lpad}{}: {value}",
            render::render_cell(&cell, styled)
        ));
        if let Some(age) = age {
            let vpad = " ".repeat(value_w - value.width() + 3);
            let age_cell = render::Cell::styled(age.clone(), anstyle::Style::new().dimmed());
            f.push_str(&vpad);
            f.push_str(&render::render_cell(&age_cell, styled));
        }
        f.push('\n');
    }
}

/// A dim trailing line reporting a transient error (last good data is kept).
fn error_trailing(msg: &str, styled: bool) -> String {
    let mut s = render::empty_line(&format!("error: {msg}"), styled);
    s.push('\n');
    s
}

/// First line of an error, truncated, for the one-line error status.
fn short_error(e: &anyhow::Error) -> String {
    let full = format!("{e:#}");
    let first = full.lines().next().unwrap_or_default();
    if first.chars().count() > 120 {
        format!("{}\u{2026}", first.chars().take(119).collect::<String>())
    } else {
        first.to_string()
    }
}

/// Restores the terminal cursor when dropped (normal return or a `?` error). A
/// matching Ctrl-C handler covers SIGINT, which skips destructors.
struct CursorGuard;

impl Drop for CursorGuard {
    fn drop(&mut self) {
        print!("{}", render::SHOW_CURSOR);
        let _ = std::io::stdout().flush();
    }
}

/// Clear the screen and paint `body`, flushing stdout.
fn repaint(body: &str) -> std::io::Result<()> {
    print!("{}{body}", render::clear());
    std::io::stdout().flush()
}

/// Entry point: authenticate, resolve repo + user, then render once or watch.
#[allow(clippy::too_many_lines)] // top-level orchestration reads better in one place
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    // Auth can drive the interactive device flow whenever there's a terminal,
    // but rendering is plain under `--once` (single-shot/scriptable output).
    let interactive = std::io::stdout().is_terminal();
    let styled = interactive && !cli.once;

    // `--demo`: render synthetic data once and exit (no auth/repo/network), so
    // the dashboard can be screenshotted. Styled on a TTY, plain when piped.
    #[cfg(feature = "demo")]
    if cli.demo {
        let sections = demo_sections();
        let changes = Changes {
            status_changed: std::collections::HashSet::from([127]),
            newly_merged: std::collections::HashSet::from([119]),
        };
        let interval = timefmt::eta(cli.interval.dur);
        let body = render_body(&sections, &cli, cli.view, &changes, interactive, None)
            + &bottom(
                "",
                &render::footer(&interval, false, interactive),
                &help_block(&cli, cli.view, !cli.no_help, interactive),
            );
        repaint(&body)?;
        return Ok(());
    }

    // Authenticate first (this may run the interactive device flow and print
    // prompts, so it must happen before we hide the cursor / clear the screen).
    let token = auth::token(cli.login, interactive)?;
    let client = Client::new(token);

    if cli.login {
        let who = client.me().context("verifying the token")?;
        eprintln!("prowl: authenticated as {who}.");
        return Ok(());
    }

    let repo = match &cli.repo {
        Some(slug) => Repo::parse(slug)?,
        None => github::detect_repo()?,
    };

    // `styled` already implies `!cli.once`, so watch mode is just `styled`.
    let watch = styled;

    // The refresh interval is constant, so both key-hint footers that carry it
    // are built once: `footer` (`r refresh (every 5m) - tab switch view - ?
    // help`) for the idle dashboard, and `footer_refreshing` (`r refreshing -
    // …`) shown while a fetch is in flight — when `r` is inert.
    let interval = timefmt::eta(cli.interval.dur);
    let footer = render::footer(&interval, false, styled);
    let footer_refreshing = render::footer(&interval, true, styled);

    // Build a frame from already-fetched data (no new fetch): the active view of
    // `good` plus the current `ui` (help/status/selection) and the given footer
    // (the `refreshing` variant while a fetch is in flight, else the idle one).
    // Used for the cached-start paint and every key repaint while idling or
    // mid-refresh, so those stay in lockstep.
    let idle_frame = |good: &Sections, ui: &Ui, footer: &str| {
        render_body(
            good,
            &cli,
            ui.view,
            &Changes::default(),
            styled,
            ui.selected,
        ) + &bottom(
            &ui.last_status,
            footer,
            &help_block(&cli, ui.view, ui.show_help, styled),
        )
    };

    // Change-detection / last-good state, seeded from the cache below so the
    // first refresh can highlight what changed while prowl wasn't running.
    let mut prev: Option<Tracker> = None;
    let mut last_good: Option<Sections> = None;
    // The interactive dashboard state: active view (from `--view`), help hidden,
    // no selection, no error line.
    let mut ui = Ui {
        view: cli.view,
        show_help: false,
        selected: None,
        last_status: String::new(),
    };

    // In watch mode, hide the cursor and quiet stdin (no echo / no line
    // buffering, but signal keys still work) for the whole session, restoring
    // both on every exit path: the guards for normal/`?` returns, the Ctrl-C
    // handler for SIGINT. Then paint instantly from the cache if we have it —
    // otherwise a loading screen — while the first live fetch runs.
    let (_cursor, _input) = if watch {
        print!("{}", render::HIDE_CURSOR);
        let input = term::quiet();
        let _ = ctrlc::set_handler(|| {
            term::restore();
            print!("{}", render::SHOW_CURSOR);
            let _ = std::io::stdout().flush();
            std::process::exit(130);
        });
        if let Some(c) = (!cli.no_cache).then(|| cache::load(&repo)).flatten() {
            repaint(&idle_frame(&c.sections, &ui, &footer))?;
            prev = Some(Tracker::build(
                c.sections.prs.as_deref(),
                c.sections.merged.as_deref(),
            ));
            last_good = Some(c.sections);
        } else {
            println!("{}{}", render::clear(), render::loading(styled));
            std::io::stdout().flush()?;
        }
        (Some(CursorGuard), input)
    } else {
        (None, None)
    };

    let me = client.me()?;
    // The default branch is the head of the "next release" commit range.
    // Resolved once; falls back to `main` if it can't be determined.
    let default_branch = client
        .default_branch(&repo)
        .unwrap_or_else(|_| "main".to_string());

    // Single render: --once, or whenever stdout is not a TTY. Only the selected
    // view's sections are fetched (you can't Tab in one-shot output).
    if cli.once || !styled {
        let sections = fetch(
            &cli,
            &client,
            &repo,
            &me,
            &default_branch,
            cli.view == View::Mine,
            cli.view == View::Reviews,
        )?;
        if !cli.no_cache {
            cache::save(&repo, &sections);
        }
        let frame = render_body(&sections, &cli, cli.view, &Changes::default(), styled, None)
            + &bottom("", "", &help_block(&cli, cli.view, !cli.no_help, styled));
        print!("{frame}");
        std::io::stdout().flush()?;
        return Ok(());
    }

    // Watch loop. Each tick clears the screen and re-renders; the bell rings
    // once when a PR of mine merges or an open PR's status changes, and those
    // rows are flagged on the redraw. A failed fetch keeps the last good data,
    // shows a dim error line, and does not ring. `armed` keeps the first
    // refresh after a cached start from ringing (it still highlights changes).
    let mut armed = false;
    loop {
        // Repaint the last-good dashboard with the `r refreshing` footer (the
        // `r` glyph dimmed): once as the fetch starts, then on every key while it
        // runs. A no-op until there's data to show. `ui` is passed in (not
        // captured) so `on_key` can still mutate it below.
        let paint_refreshing = |ui: &Ui| {
            if let Some(good) = &last_good {
                let _ = repaint(&idle_frame(good, ui, &footer_refreshing));
            }
        };
        paint_refreshing(&ui);
        // Run the blocking fetch on a worker thread and poll input while it
        // runs, so navigation, `?` (help) and Tab (switch view) stay responsive
        // mid-refresh. Both views are fetched every refresh so Tab can switch
        // instantly. A refresh is already in flight, so `Act::Refresh` (a tick
        // or `r`) is ignored; only a state change repaints (keeping the `r
        // refreshing` footer).
        let result = std::thread::scope(|scope| {
            let handle =
                scope.spawn(|| fetch(&cli, &client, &repo, &me, &default_branch, true, true));
            while !handle.is_finished() {
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(60);
                if let Act::Repaint = ui.on_key(term::wait(deadline), last_good.as_ref(), styled) {
                    paint_refreshing(&ui);
                }
            }
            handle.join().expect("fetch thread panicked")
        });
        match result {
            Ok(sections) => {
                let tracker = Tracker::build(sections.prs.as_deref(), sections.merged.as_deref());
                let changes = prev.as_ref().map(|p| tracker.diff(p)).unwrap_or_default();
                let bell = changes.any();

                ui.last_status = String::new();
                // The refreshed list may be shorter than before; keep the cursor
                // in range (or drop it if the view emptied).
                ui.selected = nav::clamp(ui.selected, nav::targets(ui.view, &sections).len());
                let body = render_body(&sections, &cli, ui.view, &changes, styled, ui.selected)
                    + &bottom(
                        "",
                        &footer,
                        &help_block(&cli, ui.view, ui.show_help, styled),
                    );
                repaint(&body)?;

                if armed && bell && !cli.no_bell {
                    render::ring_bell();
                }
                armed = true;
                if !cli.no_cache {
                    cache::save(&repo, &sections);
                }
                prev = Some(tracker);
                last_good = Some(sections);
            }
            Err(e) => {
                ui.last_status = error_trailing(&short_error(&e), styled);
                let (main, help) = match &last_good {
                    Some(good) => (
                        render_body(
                            good,
                            &cli,
                            ui.view,
                            &Changes::default(),
                            styled,
                            ui.selected,
                        ),
                        help_block(&cli, ui.view, ui.show_help, styled),
                    ),
                    None => (String::new(), String::new()),
                };
                let body = main + &bottom(&ui.last_status, &footer, &help);
                repaint(&body)?;
            }
        }
        // Wait for the interval, but let the user act now: `r` forces a refresh,
        // the movement keys drive the selection cursor, `o` opens it, `?` toggles
        // help, Tab switches view; all in place. Any other key is discarded.
        let deadline = std::time::Instant::now() + cli.interval.dur;
        loop {
            match ui.on_key(term::wait(deadline), last_good.as_ref(), styled) {
                Act::Refresh => break,
                Act::Repaint => {
                    if let Some(good) = &last_good {
                        repaint(&idle_frame(good, &ui, &footer))?;
                    }
                }
                Act::Idle => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_sections_still_show_their_headers_then_a_placeholder() {
        let cli = Cli::parse_from(["prowl"]);
        let sections = Sections {
            prs: Some(vec![]),
            queue: Some(vec![]),
            queue_next_eta: None,
            merged: Some(vec![]),
            commits: None,
            reviews: None,
            reviewed_merged: None,
        };
        let body = render_body(
            &sections,
            &cli,
            View::Mine,
            &Changes::default(),
            false,
            None,
        );

        // Each section header is present even though it has no rows...
        assert!(body.contains("My open PRs (0)"));
        assert!(body.contains("Merge Queue (0)"));
        assert!(body.contains("My merged PRs (0)"));
        // ...and the placeholder follows the header on the next line.
        let after = |title: &str, msg: &str| {
            let h = body.find(title).expect("header present");
            let p = body.find(msg).expect("placeholder present");
            assert!(p > h, "placeholder for {title} should follow its header");
        };
        after("My open PRs (0)", "No open PRs.");
        after("Merge Queue (0)", "No merge queue.");
        after("My merged PRs (0)", "No recent merged PRs.");
    }

    #[test]
    fn queue_header_shows_next_eta() {
        let cli = Cli::parse_from(["prowl"]);
        let sections = Sections {
            prs: None,
            queue: Some(vec![]),
            queue_next_eta: Some(11 * 60),
            merged: None,
            commits: None,
            reviews: None,
            reviewed_merged: None,
        };
        let body = render_body(
            &sections,
            &cli,
            View::Mine,
            &Changes::default(),
            false,
            None,
        );
        assert!(body.contains("Merge Queue (0)"));
        assert!(body.contains("~11m to merge"));
    }

    #[test]
    fn reviews_view_renders_its_own_sections() {
        let cli = Cli::parse_from(["prowl"]);
        let sections = Sections {
            prs: None,
            queue: None,
            queue_next_eta: None,
            merged: None,
            commits: None,
            reviews: Some(vec![]),
            reviewed_merged: Some(vec![]),
        };
        let body = render_body(
            &sections,
            &cli,
            View::Reviews,
            &Changes::default(),
            false,
            None,
        );
        // The Reviews view shows its two headers (not the Mine ones).
        assert!(body.contains("Reviews (0)"));
        assert!(body.contains("Reviewed & merged (0)"));
        assert!(!body.contains("My open PRs"));
    }

    #[test]
    fn selection_places_the_caret_on_the_chosen_row() {
        let cli = Cli::parse_from(["prowl"]);
        let pr = |n: i64| prs::PrRow {
            number: n,
            is_draft: false,
            title: format!("pr {n}"),
            status: None,
            merge_state: None,
            queue: None,
            fail: 0,
            url: format!("https://pr/{n}"),
            updated_at: None,
        };
        let sections = Sections {
            prs: Some(vec![pr(1), pr(2)]),
            queue: None,
            queue_next_eta: None,
            merged: None,
            commits: None,
            reviews: None,
            reviewed_merged: None,
        };
        let caret = '\u{276f}';
        // No selection -> no caret anywhere (the glanceable default).
        let none = render_body(&sections, &cli, View::Mine, &Changes::default(), true, None);
        assert!(!none.contains(caret));
        // Selecting the second row draws exactly one caret, on that row (its
        // segment precedes the #2 link).
        let sel = render_body(
            &sections,
            &cli,
            View::Mine,
            &Changes::default(),
            true,
            Some(1),
        );
        assert_eq!(sel.matches(caret).count(), 1);
        let caret_at = sel.find(caret).unwrap();
        assert!(caret_at < sel.find("#2").unwrap());
        assert!(caret_at > sel.find("#1").unwrap());
    }
}

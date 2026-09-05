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
#![allow(clippy::needless_raw_string_hashes)]
// `r#"…"#` is the convention for query blocks
// The few numeric casts are bounded/guarded (surface rows, non-negative display
// seconds); the one size-sensitive calc — the duration parser — uses checked_mul.
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::duration_suboptimal_units)] // tests spell durations in seconds on purpose

pub mod auth;
pub mod cache;
pub mod changes;
pub mod cli;
pub mod clipboard;
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
pub mod timefmt;

use anyhow::{Context, Result};
use changes::{Changes, Tracker};
use clap::Parser;
use cli::{Cli, View};
use github::{Client, Repo};
use std::borrow::Cow;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use uncurses::buffer::{Bounded, SurfaceMut, TextBuffer};
use uncurses::color::{Color, Profile};
use uncurses::event::{Event, KeyCode, KeyModifiers};
use uncurses::layout::Position;
use uncurses::program::Program;
use uncurses::screen::Screen;
use uncurses::style::Style;
use uncurses::terminal::{Stdin, Stdout, Terminal};
use uncurses::text::{Encode, TextSurface};

const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(60);
static TERMINATION_REQUESTED: AtomicBool = AtomicBool::new(false);

fn termination_requested() -> bool {
    TERMINATION_REQUESTED.load(Ordering::Acquire)
}

/// A fetched snapshot of every enabled section (`None` = section disabled).
/// Public only so the offline fixture tests and the `demo` example (which
/// renders fake data for the README screenshot) can build one.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Sections {
    pub merged: Option<Vec<merged::MergedRow>>,
    pub queue: Option<Vec<queue::QueueRow>>,
    /// Queue-level estimate: seconds until a newly added entry would merge.
    pub queue_next_eta: Option<i64>,
    pub prs: Option<Vec<prs::PrRow>>,
    pub commits: Option<commits::CommitStats>,
    /// Reviews view: open PRs awaiting / under my review.
    pub reviews: Option<Vec<reviews::ReviewRow>>,
    /// Reviews view: merged PRs I reviewed.
    pub reviewed_merged: Option<Vec<reviews::ReviewedMergedRow>>,
}

impl Sections {
    /// Every section disabled — painted as just the bottom (error/footer/help)
    /// when a fetch fails before any data has arrived.
    const EMPTY: Sections = Sections {
        merged: None,
        queue: None,
        queue_next_eta: None,
        prs: None,
        commits: None,
        reviews: None,
        reviewed_merged: None,
    };
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Visibility {
    pub(crate) prs: bool,
    pub(crate) queue: Option<queue::VisibleRows>,
    pub(crate) merged: Option<usize>,
    pub(crate) shipments: bool,
    pub(crate) reviews: bool,
    pub(crate) reviewed_merged: bool,
}

impl Visibility {
    pub(crate) fn all(sections: &Sections) -> Self {
        Self {
            prs: sections.prs.is_some(),
            queue: sections.queue.as_ref().map(|_| queue::VisibleRows::All),
            merged: sections.merged.as_ref().map(Vec::len),
            shipments: sections.commits.is_some(),
            reviews: sections.reviews.is_some(),
            reviewed_merged: sections.reviewed_merged.is_some(),
        }
    }

    fn none() -> Self {
        Self {
            prs: false,
            queue: None,
            merged: None,
            shipments: false,
            reviews: false,
            reviewed_merged: false,
        }
    }
}

#[derive(Copy, Clone)]
struct ResponsiveLayout {
    visible: Visibility,
    show_help: bool,
    constrained: bool,
    too_small: bool,
    required_height: u16,
}

fn section_height<T>(rows: Option<&[T]>, visible: bool) -> usize {
    if visible && let Some(rows) = rows {
        rows.len() + 3
    } else {
        0
    }
}

fn limited_section_height(total: usize, shown: Option<usize>) -> usize {
    shown.map_or(0, |shown| {
        if shown == 0 {
            3
        } else {
            shown + 3 + usize::from(shown < total)
        }
    })
}

fn body_height(sections: &Sections, view: View, visible: Visibility, tabs: bool) -> usize {
    let top = usize::from(tabs) * 2;
    top + match view {
        View::Mine => {
            let prs = if visible.prs {
                sections.prs.as_ref().map_or(0, |rows| {
                    if visible.queue.is_some() {
                        rows.iter().filter(|row| row.queue.is_none()).count()
                    } else {
                        rows.len()
                    }
                }) + 3
            } else {
                0
            };
            let queue = sections.queue.as_deref().map_or(0, |rows| {
                limited_section_height(rows.len(), visible.queue.map(|mode| mode.count(rows)))
            });
            let merged = sections
                .merged
                .as_deref()
                .map_or(0, |rows| limited_section_height(rows.len(), visible.merged));
            prs + queue
                + merged
                + if visible.shipments {
                    sections.commits.as_ref().map_or(0, |stats| {
                        if stats.available {
                            stats.releases.len() + 3
                        } else {
                            2
                        }
                    })
                } else {
                    0
                }
        }
        View::Reviews => {
            section_height(sections.reviews.as_deref(), visible.reviews)
                + section_height(sections.reviewed_merged.as_deref(), visible.reviewed_merged)
        }
    }
}

fn bottom_height(ui: &Ui, status: &str, footer: Option<(&str, bool)>, show_help: bool) -> usize {
    let mut height = 0;
    let mut blocks = 0usize;
    if show_help {
        height += render::help_height(ui.view);
        blocks += 1;
    }
    if !ui.search.is_empty() || ui.searching {
        height += 1;
        blocks += 1;
    }
    if !status.is_empty() {
        height += 1;
        blocks += 1;
    }
    if footer.is_some() {
        height += 1;
        blocks += 1;
    }
    height + blocks.saturating_sub(1)
}

#[allow(clippy::too_many_arguments)]
fn responsive_layout(
    width: u16,
    rows: u16,
    sections: &Sections,
    ui: &Ui,
    status: &str,
    footer: Option<(&str, bool)>,
    tabs: bool,
    pinned: bool,
) -> ResponsiveLayout {
    let all = Visibility::all(sections);
    if !pinned {
        return ResponsiveLayout {
            visible: all,
            show_help: ui.show_help,
            constrained: false,
            too_small: false,
            required_height: 0,
        };
    }

    let mut visible = all;
    let mut show_help = ui.show_help;
    let fits = |visible, show_help| {
        body_height(sections, ui.view, visible, tabs) + bottom_height(ui, status, footer, show_help)
            <= usize::from(rows)
    };

    if !fits(visible, show_help) {
        show_help = false;
    }
    if !fits(visible, show_help) {
        match ui.view {
            View::Mine => {
                let protect_shipments = !all.prs && all.queue.is_none() && all.merged.is_none();
                if !protect_shipments {
                    visible.shipments = false;
                }
                if !fits(visible, show_help)
                    && let Some(shown) = visible.merged
                {
                    for keep in (1..shown).rev() {
                        visible.merged = Some(keep);
                        if fits(visible, show_help) {
                            break;
                        }
                    }
                    if !fits(visible, show_help) {
                        visible.merged = None;
                    }
                }
                if !fits(visible, show_help)
                    && let (Some(rows), Some(_)) = (sections.queue.as_deref(), visible.queue)
                {
                    let mut count = rows.len();
                    for mode in [
                        queue::VisibleRows::BuildingAndMine,
                        queue::VisibleRows::Building,
                    ] {
                        let next = mode.count(rows);
                        if next < count {
                            visible.queue = Some(mode);
                            count = next;
                            if fits(visible, show_help) {
                                break;
                            }
                        }
                    }
                    if !fits(visible, show_help) {
                        visible.queue = None;
                    }
                }
            }
            View::Reviews => {
                if all.reviews {
                    visible.reviewed_merged = false;
                }
            }
        }
    }

    let required_height = (body_height(sections, ui.view, visible, tabs)
        + bottom_height(ui, status, footer, show_help))
    .min(usize::from(u16::MAX)) as u16;
    let too_small = width < render::MIN_WIDTH || required_height > rows;
    ResponsiveLayout {
        visible: if too_small {
            Visibility::none()
        } else {
            visible
        },
        show_help,
        constrained: show_help != ui.show_help || visible != all,
        too_small,
        required_height,
    }
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
        let (nodes, eta) = model::fetch_queue(client, repo, cli.required)?;
        (Some(queue::build_rows(nodes, me)), eta)
    } else {
        (None, None)
    };
    let prs = if want_mine && cli.show_mine() {
        let rows = prs::build_rows(model::fetch_my_prs(client, repo, me, cli.required)?);
        let rows = if cli.no_draft {
            prs::without_drafts(rows)
        } else {
            rows
        };
        Some(rows)
    } else {
        None
    };
    let commits = (want_mine && cli.show_shipments()).then_some(commit_stats);

    // Reviews view: PRs awaiting / under my review, plus merged PRs I reviewed.
    let (reviews, reviewed_merged) = if want_reviews {
        let data = model::fetch_reviews(client, repo, me, cli.review_scope.qualifier())?;
        let open = reviews::build_open_rows(data);
        let open = if cli.no_draft {
            reviews::without_drafts(open)
        } else {
            open
        };
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

/// Paint one PR section onto `s` at row `top`: a counted header (with an optional
/// dim note), then either its table or, when empty, a dim placeholder. A partial
/// section adds a dim `+N hidden` row before the trailing blank. Returns the next
/// free row.
#[allow(clippy::too_many_arguments)]
fn paint_section(
    s: &mut impl TextSurface,
    title: &str,
    accent: Color,
    count: usize,
    hidden: usize,
    note: Option<&str>,
    empty_msg: &str,
    table: Option<&render::Table>,
    alignment: &render::TableAlignment,
    ascii: bool,
    top: u16,
) -> u16 {
    let y = render::paint_header(s, title, accent, Some(&count.to_string()), note, ascii, top);
    let y = match (table, hidden) {
        (Some(table), _) => render::paint_table_aligned(s, table, alignment, ascii, y),
        (None, 0) => render::paint_dim_at(s, empty_msg, render::ROW_INDENT, y),
        (None, _) => y,
    };
    let y = if hidden > 0 {
        render::paint_dim_at(s, &format!("+{hidden} hidden"), render::ROW_INDENT, y)
    } else {
        y
    };
    y + 1
}

fn table_state(
    s: &impl TextSurface,
    tables: &[&render::Table],
) -> (render::TableAlignment, bool, u16) {
    let alignment = render::table_alignment(s, tables);
    let compact = tables
        .iter()
        .any(|table| render::table_is_compact_aligned(s, table, &alignment));
    let required_width = tables
        .iter()
        .map(|table| render::table_required_width(s, table, &alignment))
        .max()
        .unwrap_or(0);
    (alignment, compact, required_width)
}

/// Whether `table` has a `local`-th row — i.e. whether the selection landed on
/// this section. Nothing is marked on the row itself: `paint_body` highlights
/// the screen row once the whole body is painted.
fn selects_row(table: Option<&render::Table>, local: usize) -> bool {
    table.is_some_and(|t| t.rows.len() > local)
}

/// The screen row of a table's `local`-th data row, in a section painted from
/// row `top`: the section header, then the table's own header row, then the data.
/// Lets a view report where it drew the caret so the frame can scroll to it.
fn caret_row(top: u16, local: usize) -> u16 {
    top + 2 + local as u16
}

fn mine_selection(
    selected: Option<usize>,
    prs: usize,
    queue: usize,
    merged: usize,
) -> (Option<usize>, Option<usize>, Option<usize>, Option<usize>) {
    let Some(selected) = selected else {
        return (None, None, None, None);
    };
    if selected < prs {
        (Some(selected), None, None, None)
    } else if selected < prs + queue {
        (None, Some(selected - prs), None, None)
    } else if selected < prs + queue + merged {
        (None, None, Some(selected - prs - queue), None)
    } else {
        (None, None, None, Some(selected - prs - queue - merged))
    }
}

/// The Mine view: My open PRs, Merge Queue, My merged PRs, then My Shipments.
/// Each visible section shows its header (with the full count); an empty section
/// follows it with a dim placeholder, and a partial section ends with a hidden
/// count. Returns the next free row and the row the selection caret landed on,
/// if any.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn paint_mine(
    s: &mut impl TextSurface,
    sections: &Sections,
    changes: &Changes,
    selected: Option<usize>,
    ascii: bool,
    show_branch: bool,
    visible: Visibility,
    top: u16,
) -> (u16, Option<u16>, bool, u16) {
    let prs_rows = sections.prs.as_deref().filter(|_| visible.prs).map(|rows| {
        if visible.queue.is_some() {
            Cow::Owned(prs::without_queued(rows.to_vec()))
        } else {
            Cow::Borrowed(rows)
        }
    });
    let prs_table = prs_rows
        .as_deref()
        .filter(|r| !r.is_empty())
        .map(|rows| prs::to_table(rows, ascii, &changes.status_changed, show_branch));
    let queue_rows = sections
        .queue
        .as_deref()
        .zip(visible.queue)
        .map(|(rows, mode)| {
            if mode == queue::VisibleRows::All {
                Cow::Borrowed(rows)
            } else {
                Cow::Owned(mode.iter(rows).cloned().collect())
            }
        });
    let queue_table = queue_rows
        .as_deref()
        .filter(|r| !r.is_empty())
        .map(|rows| queue::to_table(rows, ascii, show_branch));
    let merged_rows = sections
        .merged
        .as_deref()
        .zip(visible.merged)
        .map(|(rows, shown)| &rows[..shown.min(rows.len())]);
    let merged_table = merged_rows
        .filter(|r| !r.is_empty())
        .map(|rows| merged::to_table(rows, ascii, &changes.newly_merged, show_branch));
    let tables: Vec<&render::Table> = [&prs_table, &queue_table, &merged_table]
        .into_iter()
        .flatten()
        .collect();
    let (alignment, compact, required_width) = table_state(s, &tables);
    if s.bounds().width < required_width {
        return (top, None, compact, required_width);
    }

    let np = prs_rows.as_deref().map_or(0, <[prs::PrRow]>::len);
    let nq = queue_rows.as_deref().map_or(0, <[queue::QueueRow]>::len);
    let nm = merged_rows.map_or(0, <[merged::MergedRow]>::len);
    let (prs_sel, queue_sel, merged_sel, ship_sel) = mine_selection(selected, np, nq, nm);
    let prs_sel = prs_sel.filter(|&local| selects_row(prs_table.as_ref(), local));
    let queue_sel = queue_sel.filter(|&local| selects_row(queue_table.as_ref(), local));
    let merged_sel = merged_sel.filter(|&local| selects_row(merged_table.as_ref(), local));

    let mut y = top;
    let mut caret = None;
    if let Some(rows) = prs_rows.as_deref() {
        caret = caret.or(prs_sel.map(|l| caret_row(y, l)));
        y = paint_section(
            s,
            "My open PRs",
            status::GREEN,
            rows.len(),
            0,
            None,
            "No open PRs.",
            prs_table.as_ref(),
            &alignment,
            ascii,
            y,
        );
    }
    if let (Some(rows), Some(shown)) = (&sections.queue, queue_rows.as_deref()) {
        // The queue-level ETA (time until a newly added entry would merge) rides
        // alongside the header as a dim note.
        caret = caret.or(queue_sel.map(|l| caret_row(y, l)));
        let eta = sections.queue_next_eta.map(|secs| {
            format!(
                "~{} to merge",
                timefmt::eta(Duration::from_secs(secs.max(0) as u64))
            )
        });
        y = paint_section(
            s,
            "Merge Queue",
            status::PEACH,
            rows.len(),
            rows.len() - shown.len(),
            eta.as_deref(),
            "No merge queue.",
            queue_table.as_ref(),
            &alignment,
            ascii,
            y,
        );
    }
    if let (Some(rows), Some(shown)) = (&sections.merged, merged_rows) {
        caret = caret.or(merged_sel.map(|l| caret_row(y, l)));
        y = paint_section(
            s,
            "My merged PRs",
            status::MAUVE,
            rows.len(),
            rows.len() - shown.len(),
            None,
            "No recent merged PRs.",
            merged_table.as_ref(),
            &alignment,
            ascii,
            y,
        );
    }
    if visible.shipments
        && let Some(stats) = &sections.commits
    {
        let (next, ship_caret) = paint_commits(s, stats, ship_sel, ascii, y);
        y = next + 1;
        caret = caret.or(ship_caret);
    }
    (y, caret, compact, required_width)
}

/// The Reviews view: PRs to review (with a per-row review-state glyph), then
/// merged PRs I reviewed. Returns the next free row, the selection caret, and
/// whether the width hid information or could not fit the mandatory columns.
fn paint_reviews(
    s: &mut impl TextSurface,
    sections: &Sections,
    selected: Option<usize>,
    ascii: bool,
    show_branch: bool,
    visible: Visibility,
    top: u16,
) -> (u16, Option<u16>, bool, u16) {
    let open_table = sections
        .reviews
        .as_ref()
        .filter(|_| visible.reviews)
        .filter(|r| !r.is_empty())
        .map(|rows| reviews::open_to_table(rows, ascii, show_branch));
    let merged_table = sections
        .reviewed_merged
        .as_ref()
        .filter(|_| visible.reviewed_merged)
        .filter(|r| !r.is_empty())
        .map(|rows| reviews::merged_to_table(rows, ascii, show_branch));
    let tables: Vec<&render::Table> = [&open_table, &merged_table].into_iter().flatten().collect();
    let (alignment, compact, required_width) = table_state(s, &tables);
    if s.bounds().width < required_width {
        return (top, None, compact, required_width);
    }

    // The open reviews come first, then the reviewed & merged rows, so a
    // selection index past the open rows indexes the latter.
    let (mut open_sel, mut merged_sel) = (None, None);
    if let Some(sel) = selected {
        let nr = if visible.reviews {
            sections.reviews.as_ref().map_or(0, Vec::len)
        } else {
            0
        };
        if sel < nr {
            open_sel = selects_row(open_table.as_ref(), sel).then_some(sel);
        } else {
            let local = sel - nr;
            merged_sel = selects_row(merged_table.as_ref(), local).then_some(local);
        }
    }

    let mut y = top;
    let mut caret = None;
    if visible.reviews
        && let Some(rows) = &sections.reviews
    {
        caret = open_sel.map(|l| caret_row(y, l));
        y = paint_section(
            s,
            "Reviews",
            status::LAVENDER,
            rows.len(),
            0,
            None,
            "No PRs to review.",
            open_table.as_ref(),
            &alignment,
            ascii,
            y,
        );
    }
    if visible.reviewed_merged
        && let Some(rows) = &sections.reviewed_merged
    {
        caret = caret.or(merged_sel.map(|l| caret_row(y, l)));
        y = paint_section(
            s,
            "Reviewed & merged",
            status::MAUVE,
            rows.len(),
            0,
            None,
            "No reviewed PRs merged recently.",
            merged_table.as_ref(),
            &alignment,
            ascii,
            y,
        );
    }
    (y, caret, compact, required_width)
}

/// Paint the dashboard's body onto `s` from row `top`: the watch-only tab strip
/// and the active view's sections. Rows that changed since the previous refresh
/// (per `changes`) are flagged with a leading marker. `tabs` is set only while
/// watching, since the view switcher is an interactive affordance. `ascii`
/// selects letters/parens over Nerd Font glyphs/bars; colors are written as
/// styles and downsampled by the surface's `Profile` at encode/render time.
///
/// Returns the next free row, the selection caret, whether width hid
/// information, and the width required by all mandatory columns.
#[allow(clippy::too_many_arguments)]
fn paint_body(
    s: &mut impl TextSurface,
    sections: &Sections,
    ui: &Ui,
    changes: &Changes,
    ascii: bool,
    tabs: bool,
    visible: Visibility,
    top: u16,
) -> (u16, Option<u16>, bool, u16) {
    let mut y = top;
    if tabs {
        y = render::paint_tabs(s, ui.view, ascii, y) + 1;
    }
    let (y, caret, compact, required_width) = match ui.view {
        View::Mine => paint_mine(
            s,
            sections,
            changes,
            ui.selected,
            ascii,
            ui.branch,
            visible,
            y,
        ),
        View::Reviews => paint_reviews(s, sections, ui.selected, ascii, ui.branch, visible, y),
    };
    // Highlight the selected row once the whole body is painted, so the bar
    // spans the content and covers the hand-laid-out shipments section too.
    if let Some(row) = caret {
        render::highlight_row(s, row);
    }
    (y, caret, compact, required_width)
}

/// Paint the dashboard's bottom block onto `s` from row `top`: the help legend,
/// the optional search prompt, `error:` line, then the footer last — the legend
/// explains the keys the footer lists, so it reads above them rather than
/// pushing them up. Each part is separated from the previous by one blank row
/// (the body's trailing blank serves as the first). While watching this block is
/// pinned to the last rows of the screen.
///
/// Returns the next free row and, while the search prompt is capturing, where
/// the terminal's own cursor should rest (relative to `s`).
#[allow(clippy::too_many_arguments)]
fn paint_bottom(
    s: &mut impl TextSurface,
    sections: &Sections,
    ui: &Ui,
    status: &str,
    footer: Option<(&str, bool)>,
    ascii: bool,
    show_help: bool,
    visible: Visibility,
    more: bool,
    top: u16,
) -> (u16, Option<Position>) {
    let mut y = top;
    let mut painted = false;
    let mut caret = None;
    if show_help {
        y = render::paint_help(s, ui.view, ascii, y);
        painted = true;
    }
    if !ui.search.is_empty() || ui.searching {
        if painted {
            y += 1;
        }
        let matches = nav::targets_visible(ui.view, sections, &ui.search, visible).len();
        let (next, at) = render::paint_search_prompt(s, &ui.search, matches, ascii, y);
        y = next;
        // Only while the prompt is capturing: with the filter merely applied,
        // the line is a static reminder and the cursor stays hidden.
        caret = ui.searching.then_some(at);
        painted = true;
    }
    if !status.is_empty() {
        if painted {
            y += 1;
        }
        y = render::paint_dim(s, status, y);
        painted = true;
    }
    if let Some((interval, refreshing)) = footer {
        if painted {
            y += 1;
        }
        y = render::paint_footer(s, interval, refreshing, more, ascii, y);
    }
    (y, caret)
}

/// Paint the body and then the bottom block right under it, as one unpinned
/// run of rows. Used for one-shot and piped output, and for the inline
/// interactive frame — anywhere the dashboard is as tall as its content rather
/// than laid out for a screen of a known height.
fn paint_dashboard(
    s: &mut impl TextSurface,
    sections: &Sections,
    ui: &Ui,
    changes: &Changes,
    status: &str,
    footer: Option<(&str, bool)>,
    ascii: bool,
) -> (u16, Option<Position>, u16) {
    let visible = Visibility::all(sections);
    let (y, _, compact, required_width) = paint_body(
        s,
        sections,
        ui,
        changes,
        ascii,
        footer.is_some(),
        visible,
        0,
    );
    let (used, caret) = paint_bottom(
        s,
        sections,
        ui,
        status,
        footer,
        ascii,
        ui.show_help,
        visible,
        compact,
        y,
    );
    (used, caret, required_width)
}

/// A safe upper bound on the dashboard body's height, used to size a surface
/// before it is cropped to the painted height.
fn height_bound(s: &Sections, ui: &Ui) -> u16 {
    // Tabs + search + error + footer + slack.
    let mut n = 10usize;
    match ui.view {
        View::Mine => {
            n += s.prs.as_ref().map_or(0, |r| r.len() + 3);
            n += s.queue.as_ref().map_or(0, |r| r.len() + 3);
            n += s.merged.as_ref().map_or(0, |r| r.len() + 3);
            // Header + one label row per bucket (upcoming + each release).
            n += s.commits.as_ref().map_or(0, |c| c.releases.len() + 4);
        }
        View::Reviews => {
            n += s.reviews.as_ref().map_or(0, |r| r.len() + 3);
            n += s.reviewed_merged.as_ref().map_or(0, |r| r.len() + 3);
        }
    }
    if ui.show_help {
        n += render::help_height(ui.view) + 1;
    }
    n as u16
}

/// Paint the one-row `Loading...` startup frame (a single dim line) and render it.
/// Shared by the watch's first paint when there's no cache and by interactive
/// `--once`, so both show the identical loading frame.
fn paint_loading(screen: &mut Screen<Stdout>) -> Result<()> {
    screen.resize((screen.width().max(1), 1));
    screen.clear();
    render::paint_dim(screen, "Loading...", 0);
    screen.render()?;
    Ok(())
}

fn staging_buffer(surface: &impl TextSurface, width: u16, height: u16) -> TextBuffer {
    TextBuffer::new(width, height)
        .with_width_mode(surface.width_mode())
        .with_eaw_wide(surface.eaw_wide())
}

fn ascii_mode(explicit: bool, profile: Profile) -> bool {
    explicit || profile == Profile::Disabled
}

/// A safe upper bound on the bottom block's height (search prompt, error line,
/// footer, help legend, and the blank row between each).
fn bottom_bound(ui: &Ui) -> u16 {
    let mut n = 6usize;
    if ui.show_help {
        n += render::help_height(ui.view) + 1;
    }
    n as u16
}

fn too_small_message(required_width: u16, required_height: u16) -> String {
    format!("Terminal too small — need {required_width}×{required_height}.")
}

fn paint_too_small(
    screen: &mut Screen<Stdout>,
    width: Option<u16>,
    required_width: u16,
    required_height: u16,
) {
    if let Some(width) = width {
        screen.resize((width, 1));
    }
    screen.clear();
    render::paint_dim(
        screen,
        &too_small_message(required_width, required_height),
        0,
    );
    screen.clear_cursor_position();
}

/// Paint the dashboard onto a `Screen` and render it.
///
/// `pinned` is the watch layout: the screen is the whole terminal, the bottom
/// block (search prompt, error line, footer, help) is glued to its last rows,
/// and the body drops lower-priority sections until it fits. Unpinned
/// — the inline interactive one-shot — the screen is instead sized to the
/// dashboard's own height and the two are painted as one run of rows.
///
/// Returns the search caret's resting cell, if the prompt is capturing.
#[allow(clippy::too_many_arguments)]
fn render_dashboard(
    screen: &mut Screen<Stdout>,
    sections: &Sections,
    ui: &Ui,
    changes: &Changes,
    status: &str,
    footer: Option<(&str, bool)>,
    ascii: bool,
    pinned: bool,
) -> Result<Option<Position>> {
    let caret = if pinned {
        // The managed area already fills the alternate screen: it is fitted on
        // entry and refitted on every resize event, so there is nothing to do
        // per frame. Refitting here would only re-query the terminal size on
        // every redraw (and, before uncurses 0.0.2, force a clear + full
        // repaint — the flicker this replaced).
        let (w, rows) = (screen.width().max(1), screen.height().max(1));
        let layout = responsive_layout(w, rows, sections, ui, status, footer, true, true);
        if layout.too_small {
            paint_too_small(screen, None, render::MIN_WIDTH, layout.required_height);
            screen.render()?;
            return Ok(None);
        }

        // Body and bottom are painted into their own buffers because the frame
        // places them independently and pins the bottom to the last rows.
        let mut body = staging_buffer(screen, w, height_bound(sections, ui).max(1));
        let (body_h, sel, compact, required_width) = paint_body(
            &mut body,
            sections,
            ui,
            changes,
            ascii,
            true,
            layout.visible,
            0,
        );
        let required_width = required_width.max(render::MIN_WIDTH);
        if w < required_width {
            paint_too_small(screen, None, required_width, layout.required_height);
            screen.render()?;
            return Ok(None);
        }
        let mut bottom = staging_buffer(screen, w, bottom_bound(ui).max(1));
        let (bottom_h, at) = paint_bottom(
            &mut bottom,
            sections,
            ui,
            status,
            footer,
            ascii,
            layout.show_help,
            layout.visible,
            layout.constrained || compact,
            0,
        );

        screen.clear();
        let (top, cut) =
            render::compose(screen, &mut body, body_h, &mut bottom, bottom_h, rows, sel);
        // The prompt's caret is relative to the bottom block, which just moved —
        // and whose head may have been cut off on a short terminal, taking the
        // prompt with it.
        at.filter(|p| p.y >= cut)
            .map(|p| Position::new(p.x, top + (p.y - cut)))
    } else {
        let w = screen.width().max(1);
        let required_height = (body_height(
            sections,
            ui.view,
            Visibility::all(sections),
            footer.is_some(),
        ) + bottom_height(ui, status, footer, ui.show_help))
        .min(usize::from(u16::MAX)) as u16;
        if w < render::MIN_WIDTH {
            paint_too_small(screen, Some(w), render::MIN_WIDTH, required_height);
            None
        } else {
            // Grow tall enough to paint everything, paint, then shrink to the height
            // actually used so the surface is exactly the dashboard's line count.
            screen.resize((w, (height_bound(sections, ui) + bottom_bound(ui)).max(1)));
            screen.clear();
            let (used, caret, required_width) =
                paint_dashboard(screen, sections, ui, changes, status, footer, ascii);
            let required_width = required_width.max(render::MIN_WIDTH);
            if w >= required_width {
                screen.resize((w, used.max(1)));
                caret
            } else {
                paint_too_small(screen, Some(w), required_width, required_height);
                None
            }
        }
    };
    // Steer the terminal's own cursor to the prompt, so the search line gets a
    // real (blinking, shape-honoring) cursor instead of a painted stand-in.
    match caret {
        Some(pos) => screen.set_cursor_position(pos),
        None => screen.clear_cursor_position(),
    }
    screen.render()?;
    Ok(caret)
}

/// Paint a whole dashboard onto an offscreen [`TextBuffer`] sized to its content
/// and encode it, `profile` deciding how much styling survives (`Disabled` drops
/// SGR and hyperlinks, so piped output is plain). What `--once` writes to the
/// terminal, and what the `demo` example renders fake data through, so the
/// README screenshot can't drift from the real layout.
pub fn render_to_string(
    sections: &Sections,
    ui: &Ui,
    changes: &Changes,
    footer: Option<(&str, bool)>,
    ascii: bool,
    profile: Profile,
) -> String {
    let w = render::OUTPUT_WIDTH as u16;
    let mut canvas = TextBuffer::new(w, height_bound(sections, ui) + bottom_bound(ui));
    // One-shot output never searches, so there is no caret to place.
    let (used, _, required_width) =
        paint_dashboard(&mut canvas, sections, ui, changes, "", footer, ascii);
    let required_width = required_width.max(render::MIN_WIDTH);
    if w >= required_width {
        canvas.resize(w, used.max(1));
    } else {
        canvas = TextBuffer::new(w, 1);
        render::paint_dim(&mut canvas, &too_small_message(required_width, used), 0);
    }

    let mut out = Vec::new();
    canvas
        .encode_with(&mut out, profile)
        .expect("encoding to a Vec cannot fail");
    String::from_utf8(out).expect("uncurses encodes valid UTF-8")
}

/// Render the dashboard once into an offscreen [`TextBuffer`] sized to its content,
/// then encode it to the terminal's output with the **detected** color profile
/// (plain when piped) and exit. Used by `--once` and non-TTY output.
fn render_once(
    terminal: &Terminal<Stdin, Stdout>,
    sections: &Sections,
    cli: &Cli,
    changes: &Changes,
    footer: Option<(&str, bool)>,
) -> Result<()> {
    let profile = Profile::detect_from(terminal.env(), terminal.is_terminal().1);
    let ascii = ascii_mode(cli.ascii, profile);
    // One-shot output has no interaction: no tabs, no selection, no search; the
    // help legend follows `--no-help` instead of the `?` toggle.
    let ui = Ui::once(cli);
    let painted = render_to_string(sections, &ui, changes, footer, ascii, profile);

    // A closed downstream pipe (`prowl --once | head`) is a clean exit, not an
    // error worth printing.
    let mut out = terminal.output();
    let write = out
        .write_all(painted.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush());
    match write {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Start an interactive program and restore it if any setup step fails after
/// entering raw mode.
fn start_program(terminal: Terminal<Stdin, Stdout>) -> Result<Program<Stdin, Stdout>> {
    let mut program = Program::new(terminal)?;
    let setup = (|| -> std::io::Result<()> {
        program.init()?;
        // `init` no longer probes the terminal, so ask: the reply is what lets
        // the renderer bracket each frame in synchronized output.
        program.query_capabilities(&[])?;
        program.hide_cursor()
    })();
    if let Err(error) = setup {
        let _ = program.finish();
        return Err(error.into());
    }
    Ok(program)
}

fn run_once_session(
    program: &mut Program<Stdin, Stdout>,
    cli: &Cli,
    client: &Client,
    repo: &Repo,
) -> Result<Option<Sections>> {
    // Inline loading frame; raw mode swallows keystrokes so nothing echoes into
    // the output while we wait.
    paint_loading(program.screen_mut())?;

    // Fetch off-thread so `q` stays live during network I/O. `me` and the
    // default branch are resolved here too, so even the first round-trip never
    // blocks the abort key.
    let (cli2, client2, repo2) = (cli.clone(), client.clone(), repo.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let fetched = (|| {
            let me = client2.me()?;
            let default_branch = client2
                .default_branch(&repo2)
                .unwrap_or_else(|_| "main".to_string());
            // Only the selected view: `--once` output has no Tab to switch.
            fetch(
                &cli2,
                &client2,
                &repo2,
                &me,
                &default_branch,
                cli2.view == View::Mine,
                cli2.view == View::Reviews,
            )
        })();
        let _ = tx.send(fetched); // ignored if we already aborted (rx dropped)
    });

    // `None` => the user aborted; `Some(result)` => the fetch finished.
    let fetched = loop {
        if termination_requested() {
            break None;
        }
        match rx.try_recv() {
            Ok(result) => break Some(result),
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                break Some(Err(anyhow::anyhow!("fetch worker stopped unexpectedly")));
            }
        }
        if program.poll_event(Some(INPUT_POLL_INTERVAL))? {
            let mut aborted = false;
            // Reading observes the event, so capability replies (synchronized
            // output) are tracked as they pass through.
            while let Some(ev) = program.try_read_event()? {
                if let Action::Quit = classify(&ev) {
                    aborted = true;
                }
            }
            if aborted {
                break None;
            }
        }
    };

    match fetched {
        Some(Ok(sections)) => {
            // Replace the loading frame with the dashboard, then leave it inline.
            let ascii = ascii_mode(cli.ascii, program.screen().color_profile());
            render_dashboard(
                program.screen_mut(),
                &sections,
                &Ui::once(cli),
                &Changes::default(),
                "",
                None,
                ascii,
                false,
            )?;
            Ok(Some(sections))
        }
        Some(Err(error)) => Err(error),
        None => {
            // Aborted: wipe the loading frame so nothing is left behind.
            program.screen_mut().clear();
            program.screen_mut().render()?;
            Ok(None)
        }
    }
}

fn run_once_interactive(
    terminal: Terminal<Stdin, Stdout>,
    cli: &Cli,
    client: &Client,
    repo: &Repo,
) -> Result<()> {
    let mut program = start_program(terminal)?;
    let result = run_once_session(&mut program, cli, client, repo);
    let finish = program.finish();
    let sections = match result {
        Ok(sections) => {
            finish?;
            sections
        }
        Err(error) => {
            let _ = finish;
            return Err(error);
        }
    };
    if let Some(sections) = sections
        && !cli.no_cache
    {
        cache::save(repo, cli.required, &sections);
    }
    Ok(())
}

/// Paint the "My Shipments" section onto `s` at row `top`: my commit counts for
/// the next (unreleased) version and the last few stable releases, one labelled
/// row each with the labels right-aligned so the colons and counts line up. Each
/// label links out — the upcoming one to the compare log, each release to its
/// release page — and shipped releases also show how long ago they were
/// published, aligned into a trailing column. Returns the next row and the row
/// the selection caret was painted on, if any.
fn paint_commits(
    s: &mut impl TextSurface,
    stats: &commits::CommitStats,
    selected: Option<usize>,
    ascii: bool,
    top: u16,
) -> (u16, Option<u16>) {
    if !stats.available {
        // The shipments rows lead with a 2-column gutter, not a table's marker
        // and glyph cells, so the placeholder follows that instead.
        return (
            render::paint_dim_at(s, "Commit stats unavailable.", 2, top),
            None,
        );
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
    let mut y = render::paint_header(
        s,
        "My Shipments",
        status::BLUE,
        Some(&total),
        None,
        ascii,
        top,
    );

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
        rows.push((
            r.tag.clone(),
            Some(r.bucket.url.clone()),
            value(Some(&r.bucket)),
            r.published_at.as_deref().map(|p| timefmt::age_of(Some(p))),
        ));
    }

    // Right-align the labels and pad the counts to shared widths, so the colons,
    // counts, and publish ages each line up in a readable column.
    let label_w = rows
        .iter()
        .map(|(l, ..)| s.str_width(l) as usize)
        .max()
        .unwrap_or(0);
    let value_w = rows
        .iter()
        .map(|(.., v, _)| s.str_width(v) as usize)
        .max()
        .unwrap_or(0);

    // The selection index counts only navigable (URL-bearing) rows; the sole
    // url-less row is a commit-less "upcoming", which then shifts the rendered
    // caret row down by one.
    let sel_row = selected.map(|k| if stats.upcoming.is_some() { k } else { k + 1 });

    let mut caret = None;
    for (i, (label, url, value, age)) in rows.iter().enumerate() {
        // The first row is the upcoming (unreleased) version; set it apart in
        // italics. The label links to the bucket's log/release page.
        let style = if i == 0 && !ascii {
            Style::new().italic()
        } else {
            Style::new()
        };
        let cell = match url {
            Some(url) => render::Cell::link_styled(label.clone(), url.clone(), style),
            None => render::Cell::styled(label.clone(), style),
        };
        // A 2-column leading gutter keeps the labels aligned; the selected row
        // is reported here and highlighted by `paint_body` once the body is done.
        if Some(i) == sel_row {
            caret = Some(y);
        }
        let x = (2 + label_w - s.str_width(label) as usize) as u16;
        let p = s.set_str((x, y), &cell.text, &cell.style);
        let p = s.set_str((p.x, y), &format!(": {value}"), None);
        if let Some(age) = age {
            let x = p.x + (value_w - s.str_width(value) as usize + 3) as u16;
            s.set_str((x, y), age, Style::new().faint());
        }
        y += 1;
    }
    (y, caret)
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

/// What a keypress or resize means to the watch loop in normal (non-search) mode.
enum Action {
    /// Ignore (an unbound key, or a non-input event).
    None,
    /// `q`/`Ctrl-C`: quit.
    Quit,
    /// `r`/`R`: refresh now.
    Refresh,
    /// `?`: toggle the help legend.
    ToggleHelp,
    /// `Tab`: switch to the other view.
    SwitchView,
    /// `Enter`: open the selected row in the browser.
    Open,
    /// `y`: copy the selected row's link.
    Copy,
    /// `Y`: copy every link in the section the cursor is in.
    CopySection,
    /// `/`: open the search prompt.
    Search,
    /// `Esc`: clear an applied filter, or quit when there is none.
    Cancel,
    /// A movement key: move the selection cursor.
    Move(nav::Move),
    /// `Ctrl-Z`: suspend to the shell, then resume.
    Suspend,
    /// The terminal was resized to these cell dimensions.
    Resize(u16, u16),
}

/// A keystroke while the search prompt is open (raw text input, unlike the
/// semantic [`Action`]s of normal mode).
#[derive(Debug, PartialEq, Eq)]
enum SearchAction {
    /// Ignore (an unbound key, or a non-input event).
    None,
    /// A printable character to append to the query.
    Char(char),
    /// Backspace: drop the last query character.
    Backspace,
    /// Enter: apply the filter and leave the prompt.
    Enter,
    /// Esc: clear the filter and leave the prompt.
    Esc,
    /// `Ctrl-C`: quit, including while the prompt is open.
    Quit,
    /// `Ctrl-Z`: suspend to the shell, then resume.
    Suspend,
    /// The terminal was resized to these cell dimensions.
    Resize(u16, u16),
}

/// Classify an event into a normal-mode [`Action`]. In raw mode the signal keys
/// arrive as ordinary key events, so `ctrl+c`/`ctrl+z` are matched here rather
/// than through signal handlers. `Key::matches` is case-sensitive, so the
/// case-insensitive bindings list both forms.
fn classify(ev: &Event) -> Action {
    match ev {
        Event::KeyPress(k) => {
            if k.matches_any(["q", "Q", "ctrl+c"]) {
                Action::Quit
            } else if k.matches("esc") {
                Action::Cancel
            } else if k.matches_any(["r", "R"]) {
                Action::Refresh
            } else if k.matches("?") {
                Action::ToggleHelp
            } else if k.matches("tab") {
                Action::SwitchView
            } else if k.matches("enter") {
                Action::Open
            } else if k.matches("y") {
                Action::Copy
            } else if k.matches("Y") {
                Action::CopySection
            } else if k.matches("/") {
                Action::Search
            } else if k.matches("ctrl+z") {
                Action::Suspend
            } else if k.matches_any(["j", "down"]) {
                Action::Move(nav::Move::Down)
            } else if k.matches_any(["k", "up"]) {
                Action::Move(nav::Move::Up)
            } else if k.matches("g") {
                Action::Move(nav::Move::Top)
            } else if k.matches("G") {
                Action::Move(nav::Move::Bottom)
            } else if k.matches("ctrl+d") {
                Action::Move(nav::Move::HalfDown)
            } else if k.matches("ctrl+u") {
                Action::Move(nav::Move::HalfUp)
            } else {
                Action::None
            }
        }
        Event::Resize(ws) => Action::Resize(ws.col, ws.row),
        _ => Action::None,
    }
}

/// Classify an event while the search prompt is open: printable characters
/// extend the query, everything else is an edit/exit key. `q` remains a
/// searchable character, while `Ctrl-C` quits and Esc closes the prompt.
fn classify_search(ev: &Event) -> SearchAction {
    match ev {
        Event::KeyPress(k) => {
            if k.matches("ctrl+c") {
                SearchAction::Quit
            } else {
                match k.code {
                    KeyCode::Char(c)
                        if !k
                            .modifiers
                            .intersects(KeyModifiers::CTRL | KeyModifiers::ALT) =>
                    {
                        SearchAction::Char(c)
                    }
                    KeyCode::Space => SearchAction::Char(' '),
                    KeyCode::Backspace => SearchAction::Backspace,
                    KeyCode::Enter => SearchAction::Enter,
                    KeyCode::Escape => SearchAction::Esc,
                    _ if k.matches("ctrl+z") => SearchAction::Suspend,
                    _ => SearchAction::None,
                }
            }
        }
        Event::Resize(ws) => SearchAction::Resize(ws.col, ws.row),
        _ => SearchAction::None,
    }
}

/// What the watch loop should do after handling a batch of input.
enum Flow {
    /// Keep waiting / keep fetching.
    Continue,
    /// `r` was pressed: refresh now.
    Refresh,
    /// A quit key was pressed: leave the loop (the caller tears the screen down).
    Quit,
}

/// The interactive dashboard state threaded through painting and mutated on each
/// keypress. One-shot output uses the inert [`Ui::once`] form. Public only so the
/// `demo` example (which renders fake data for the README screenshot) can build
/// one.
pub struct Ui {
    /// Active view; starts at `--view`, toggled with Tab.
    pub view: View,
    /// Whether the `?` help legend is shown (starts hidden while watching).
    pub show_help: bool,
    /// Navigation cursor into the active view's (filtered) rows — lazy (`None`
    /// until the user moves it), reset when switching views or changing the
    /// search, and restored by URL after a refresh or resize.
    pub selected: Option<usize>,
    /// The active search query; empty means no filter is applied.
    pub search: String,
    /// Whether the search prompt is open and capturing text.
    pub searching: bool,
    /// `--branch`: show each PR's head branch.
    pub branch: bool,
}

impl Ui {
    /// The non-interactive form used by `--once` / piped output: the `--view`
    /// sections, the help legend per `--no-help`, no selection and no search.
    pub fn once(cli: &Cli) -> Ui {
        Ui {
            view: cli.view,
            show_help: !cli.no_help,
            selected: None,
            search: String::new(),
            searching: false,
            branch: cli.branch,
        }
    }

    /// The sections to paint: `good` filtered by the active search, or `good`
    /// itself when there's no query. `buf` owns the filtered copy if one is made,
    /// so the returned reference stays valid for the caller.
    fn shown<'a>(&self, good: &'a Sections, buf: &'a mut Option<Sections>) -> &'a Sections {
        if self.search.is_empty() {
            good
        } else {
            buf.insert(nav::filter(good, &self.search))
        }
    }
}

/// Entry point: authenticate, resolve repo + user, then render once or watch.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    // Detect interactivity through uncurses' `Terminal` (is the output half a
    // TTY?) and reuse the very same handle to build the watch `Screen` or to
    // encode the one-shot frame. Auth can drive the interactive device flow
    // whenever there's a terminal.
    let terminal = Terminal::stdio();
    let interactive = terminal.is_terminal().1;

    // Authenticate first (this may run the interactive device flow and print
    // prompts, so it must happen before we enter the alternate screen).
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

    // Non-interactive (piped, redirected, not a TTY): a blocking fetch, encode
    // the frame to stdout, and exit. No screen, no loading UI.
    if !interactive {
        let me = client.me()?;
        let default_branch = client
            .default_branch(&repo)
            .unwrap_or_else(|_| "main".to_string());
        // Only the selected view's sections are fetched (you can't Tab in
        // one-shot output).
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
            cache::save(&repo, cli.required, &sections);
        }
        return render_once(&terminal, &sections, &cli, &Changes::default(), None);
    }

    ctrlc::set_handler(|| TERMINATION_REQUESTED.store(true, Ordering::Release))
        .context("installing termination handler")?;
    let result = if cli.once {
        // An inline screen swallows input while the fetch runs, then leaves the
        // dashboard in the terminal.
        run_once_interactive(terminal, &cli, &client, &repo)
    } else {
        // `stop` always runs, so `Program::finish` restores the terminal after
        // a clean quit or an error from the event loop.
        let mut app = App::start(terminal, &cli, &client, &repo)?;
        let result = app.run();
        app.stop()?;
        result
    };
    if termination_requested() {
        std::process::exit(130);
    }
    result
}

/// The interactive watch, following the uncurses example `App` pattern: it owns
/// the `Screen` and all dashboard state. `start` brings the terminal up, `run`
/// drives the refresh + event loop (returning `Ok(())` when a quit key is
/// pressed), and `stop` tears it back down with `Program::finish`. The caller
/// always calls `stop`, so the terminal is restored on every path.
struct App<'a> {
    program: Program<Stdin, Stdout>,
    cli: &'a Cli,
    client: &'a Client,
    repo: &'a Repo,
    me: String,
    default_branch: String,
    /// The constant next-refresh ETA shown in the key-hint footer.
    eta: String,
    /// Change-detection baseline and the last successfully fetched sections.
    prev: Option<Tracker>,
    last_good: Option<Sections>,
    /// The interactive dashboard state: view, help visibility, selection, search.
    ui: Ui,
    /// The most recent short error (empty unless a refresh or an open failed),
    /// kept so a `?` toggle or a repaint keeps it on screen.
    /// The dim trailing line above the footer: a refresh/open error, or a
    /// transient note (a clipboard copy). Worded in full, and cleared by the
    /// next refresh.
    last_status: String,
    /// Whether a fetch is in flight, so the footer can say `r refreshing`.
    refreshing: bool,
    /// Whether the bell is armed. The first refresh after a cached start is
    /// silent (it still highlights changes).
    armed: bool,
    /// Whether we've switched from the inline loading frame to the alternate
    /// screen. The watch starts inline and enters the alt screen once the first
    /// fetch lands (or immediately when there's a cache to paint).
    in_alt: bool,
    /// Whether the terminal cursor is currently shown. `show_cursor`/
    /// `hide_cursor` always emit, so track the state and only toggle on a
    /// change — the cursor is shown solely while the search prompt captures.
    cursor_shown: bool,
}

impl<'a> App<'a> {
    /// Bring the terminal up (raw mode, hidden cursor) from the supplied
    /// `Terminal` — the screen keeps the terminal's detected color profile. The
    /// loading frame shows **inline**; the alt screen is entered once the first
    /// fetch lands (or immediately when there's a cache to paint), so loading
    /// looks like ordinary command output before the dashboard takes over.
    fn start(
        terminal: Terminal<Stdin, Stdout>,
        cli: &'a Cli,
        client: &'a Client,
        repo: &'a Repo,
    ) -> Result<Self> {
        let program = start_program(terminal)?;

        let mut app = App {
            eta: timefmt::eta(cli.interval.dur),
            program,
            cli,
            client,
            repo,
            me: String::new(),
            default_branch: String::new(),
            prev: None,
            last_good: None,
            ui: Ui {
                view: cli.view,
                show_help: false,
                selected: None,
                search: String::new(),
                searching: false,
                branch: cli.branch,
            },
            last_status: String::new(),
            refreshing: false,
            armed: false,
            in_alt: false,
            cursor_shown: false,
        };

        // If the very first paint fails, restore the terminal before bailing
        // (`stop` handles both the inline and alt-screen states).
        if let Err(e) = app.paint_startup() {
            let _ = app.stop();
            return Err(e);
        }
        Ok(app)
    }

    /// The initial cache/loading paint, seeding change-detection from the cache
    /// so the first live refresh highlights what changed while prowl was away.
    fn paint_startup(&mut self) -> Result<()> {
        match (!self.cli.no_cache)
            .then(|| cache::load(self.repo, self.cli.required))
            .flatten()
        {
            Some(c) => {
                self.prev = Some(Tracker::build(
                    c.sections.prs.as_deref(),
                    c.sections.merged.as_deref(),
                ));
                self.last_good = Some(c.sections);
                // Cached data is real content, so go straight to the alt screen.
                self.enter_alt()?;
                self.redraw(&Changes::default())?;
            }
            None => paint_loading(self.program.screen_mut())?,
        }
        Ok(())
    }

    /// Switch from the inline loading frame to the alternate screen, once. The
    /// inline frame is dropped to zero rows and flushed first, so taking over the
    /// screen leaves the terminal as it was before prowl ran.
    fn enter_alt(&mut self) -> Result<()> {
        if !self.in_alt {
            let w = self.program.screen().width().max(1);
            self.program.screen_mut().resize((w, 0));
            self.program.screen_mut().render()?;
            self.program.enter_alt_screen()?;
            // The one place `autoresize` is the right tool: we now own the
            // whole window, and it is the only call that queries the terminal
            // for its row count — which we need, having just collapsed the
            // managed area to zero rows. Resize events carry their own size.
            self.program.autoresize()?;
            self.in_alt = true;
        }
        Ok(())
    }

    /// Paint the current dashboard via [`render_dashboard`], drawing the last
    /// good sections (or an empty frame, so a first-fetch error still shows its
    /// error line + footer) with `changes` highlighted.
    fn redraw(&mut self, changes: &Changes) -> Result<()> {
        let good = self.last_good.as_ref().unwrap_or(&Sections::EMPTY);
        let mut buf = None;
        let sections = self.ui.shown(good, &mut buf);
        let ascii = ascii_mode(self.cli.ascii, self.program.screen().color_profile());
        let caret = render_dashboard(
            self.program.screen_mut(),
            sections,
            &self.ui,
            changes,
            &self.last_status,
            Some((self.eta.as_str(), self.refreshing)),
            ascii,
            // Pinning lays the frame out for a screen of a known height, which
            // is only true once we own the alternate screen.
            self.in_alt,
        )?;
        // Reveal the cursor only once it's parked in the prompt, so it never
        // blinks at a stale cell.
        let want = caret.is_some();
        if want != self.cursor_shown {
            if want {
                self.program.show_cursor()?;
            } else {
                self.program.hide_cursor()?;
            }
            self.cursor_shown = want;
        }
        Ok(())
    }

    /// Drive the watch: loop fetch → paint → wait, returning `Ok(())` when the
    /// user presses a quit key.
    fn run(&mut self) -> Result<()> {
        loop {
            if termination_requested() {
                return Ok(());
            }
            if let Flow::Quit = self.fetch_responsive()? {
                return Ok(());
            }
            if let Flow::Quit = self.wait_interval()? {
                return Ok(());
            }
        }
    }

    /// Tear the terminal back down. The consuming `Program::finish` is the
    /// idiomatic teardown: it exits the alternate screen, shows the cursor, and
    /// leaves raw mode.
    fn stop(self) -> Result<()> {
        self.program.finish()?;
        Ok(())
    }

    /// Fetch on a detached background thread while the main thread keeps polling
    /// input, so quit/`?`/resize stay live and no network I/O ever blocks the UI.
    /// The result arrives over a channel; pressing quit returns immediately and
    /// abandons the in-flight request (the thread is reaped at process exit).
    /// `me` and the default branch are resolved here too (once), so even the
    /// first round-trip never freezes input. `r` is ignored — a fetch is already
    /// in flight.
    fn fetch_responsive(&mut self) -> Result<Flow> {
        // The footer says `r refreshing` (with `r` dimmed) for the duration.
        self.refreshing = true;
        self.repaint_last()?;
        let flow = self.fetch_loop();
        self.refreshing = false;
        flow
    }

    /// The fetch + input-poll loop itself; [`Self::fetch_responsive`] wraps it to
    /// keep the `refreshing` footer state balanced on every exit path.
    fn fetch_loop(&mut self) -> Result<Flow> {
        let (cli, client, repo) = (self.cli.clone(), self.client.clone(), self.repo.clone());
        let mut me = self.me.clone();
        let mut default_branch = self.default_branch.clone();
        let resolve = me.is_empty();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let fetched = (|| {
                if resolve {
                    me = client.me()?;
                    default_branch = client
                        .default_branch(&repo)
                        .unwrap_or_else(|_| "main".to_string());
                }
                // Both views every refresh, so Tab switches instantly.
                let sections = fetch(&cli, &client, &repo, &me, &default_branch, true, true)?;
                Ok((me, default_branch, sections))
            })();
            let _ = tx.send(fetched); // ignored if we already quit (rx dropped)
        });

        loop {
            if termination_requested() {
                return Ok(Flow::Quit);
            }
            match rx.try_recv() {
                Ok(Ok((me, default_branch, sections))) => {
                    self.me = me;
                    self.default_branch = default_branch;
                    // Cleared before painting, so the result frame already shows
                    // the plain `r refresh` hint again.
                    self.refreshing = false;
                    self.apply(sections)?;
                    return Ok(Flow::Continue);
                }
                Ok(Err(e)) => {
                    self.refreshing = false;
                    self.show_error(e)?;
                    return Ok(Flow::Continue);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(Flow::Continue),
            }
            if self.program.poll_event(Some(INPUT_POLL_INTERVAL))? {
                while let Some(ev) = self.program.try_read_event()? {
                    if let Flow::Quit = self.handle_event(&ev)? {
                        return Ok(Flow::Quit);
                    }
                }
            }
        }
    }

    /// Wait out the refresh interval, staying responsive: `r` refreshes now, `?`
    /// toggles help, quit/suspend/resize are honored, other keys are discarded.
    fn wait_interval(&mut self) -> Result<Flow> {
        let deadline = Instant::now() + self.cli.interval.dur;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            if termination_requested() {
                return Ok(Flow::Quit);
            }
            if !self
                .program
                .poll_event(Some(remaining.min(INPUT_POLL_INTERVAL)))?
            {
                continue;
            }
            while let Some(ev) = self.program.try_read_event()? {
                match self.handle_event(&ev)? {
                    Flow::Quit => return Ok(Flow::Quit),
                    Flow::Refresh => return Ok(Flow::Continue), // refresh now
                    Flow::Continue => {}
                }
            }
        }
        Ok(Flow::Continue)
    }

    /// Apply an input event's side effects (navigation, view switch, search,
    /// open, suspend, help toggle, resize repaint) and report the control flow it
    /// implies. While the search prompt is open every keystroke is text, so it is
    /// routed to [`Self::handle_search_event`] instead.
    fn handle_event(&mut self, ev: &Event) -> Result<Flow> {
        if self.ui.searching {
            return self.handle_search_event(ev);
        }
        Ok(match classify(ev) {
            Action::Quit => Flow::Quit,
            Action::Refresh => Flow::Refresh,
            Action::Suspend => {
                self.suspend()?;
                Flow::Continue
            }
            Action::ToggleHelp => {
                self.ui.show_help = !self.ui.show_help;
                self.ui.selected = None;
                self.repaint_last()?;
                Flow::Continue
            }
            Action::SwitchView => {
                // Selection indices don't carry across views, so start fresh.
                self.ui.view = self.ui.view.toggle();
                self.ui.selected = None;
                self.repaint_last()?;
                Flow::Continue
            }
            Action::Search => {
                self.ui.searching = true;
                self.ui.selected = None;
                self.repaint_last()?;
                Flow::Continue
            }
            Action::Cancel => {
                // Esc clears an applied filter; with none to clear, it quits.
                if self.ui.search.is_empty() {
                    Flow::Quit
                } else {
                    self.ui.search.clear();
                    self.ui.selected = None;
                    self.repaint_last()?;
                    Flow::Continue
                }
            }
            Action::Open => {
                self.open_selected()?;
                Flow::Continue
            }
            Action::Copy => {
                self.copy_selected()?;
                Flow::Continue
            }
            Action::CopySection => {
                self.copy_section()?;
                Flow::Continue
            }
            Action::Move(m) => {
                let len = self.target_count();
                let next = nav::moved(m, self.ui.selected, len, self.half_page());
                if next != self.ui.selected {
                    self.ui.selected = next;
                    self.repaint_last()?;
                }
                Flow::Continue
            }
            Action::Resize(w, h) => {
                let selected = self.selected_url();
                // The event already carries the new size, so resize to it
                // directly rather than re-querying the terminal. Only the alt
                // screen is the whole window: inline (the loading frame) the
                // managed area keeps its own height and just follows the width.
                let h = if self.in_alt {
                    h
                } else {
                    self.program.screen().height()
                };
                self.program.screen_mut().resize((w, h));
                self.restore_selection(selected.as_deref());
                self.repaint_last()?;
                Flow::Continue
            }
            Action::None => Flow::Continue,
        })
    }

    /// Apply a keystroke while the search prompt is open. Typing filters live
    /// (resetting the cursor), Enter applies the filter and closes the prompt,
    /// Esc clears the filter and closes it.
    fn handle_search_event(&mut self, ev: &Event) -> Result<Flow> {
        match classify_search(ev) {
            SearchAction::Char(c) => {
                self.ui.search.push(c);
                self.ui.selected = None;
            }
            SearchAction::Backspace => {
                self.ui.search.pop();
                self.ui.selected = None;
            }
            SearchAction::Enter => {
                self.ui.searching = false;
            }
            SearchAction::Esc => {
                self.ui.search.clear();
                self.ui.searching = false;
            }
            SearchAction::Quit => return Ok(Flow::Quit),
            SearchAction::Suspend => return self.suspend().map(|()| Flow::Continue),
            // The prompt only opens while watching, so we own the alt screen
            // and the frame is the whole window.
            SearchAction::Resize(w, h) => {
                self.program.screen_mut().resize((w, h));
            }
            SearchAction::None => return Ok(Flow::Continue),
        }
        self.repaint_last()?;
        Ok(Flow::Continue)
    }

    /// Suspend to the shell (Ctrl-Z) and repaint on resume — the canvas may not
    /// survive the stop, so don't rely on `resume`'s flush. `SIGTSTP` is Unix
    /// job control, so elsewhere Ctrl-Z just repaints.
    fn suspend(&mut self) -> Result<()> {
        #[cfg(unix)]
        {
            self.program.suspend()?;
            self.program.resume()?;
        }
        self.repaint_last()
    }

    /// How many rows the selection cursor can visit in the active view, with the
    /// current filter applied.
    fn target_count(&self) -> usize {
        let Some(good) = &self.last_good else {
            return 0;
        };
        let mut filtered = None;
        let shown = self.ui.shown(good, &mut filtered);
        let visible = self.visible_sections(shown);
        nav::targets_visible(self.ui.view, shown, &self.ui.search, visible).len()
    }

    fn visible_sections(&self, sections: &Sections) -> Visibility {
        responsive_layout(
            self.program.screen().width().max(1),
            self.program.screen().height().max(1),
            sections,
            &self.ui,
            &self.last_status,
            Some((self.eta.as_str(), self.refreshing)),
            true,
            self.in_alt,
        )
        .visible
    }

    fn selected_url(&self) -> Option<String> {
        let selected = self.ui.selected?;
        self.last_good.as_ref().and_then(|good| {
            let mut filtered = None;
            let shown = self.ui.shown(good, &mut filtered);
            let visible = self.visible_sections(shown);
            nav::targets_visible(self.ui.view, shown, &self.ui.search, visible)
                .get(selected)
                .map(|url| (*url).to_string())
        })
    }

    fn restore_selection(&mut self, url: Option<&str>) {
        self.ui.selected = url.and_then(|url| {
            self.last_good.as_ref().and_then(|good| {
                let mut filtered = None;
                let shown = self.ui.shown(good, &mut filtered);
                let visible = self.visible_sections(shown);
                nav::target_index(self.ui.view, shown, &self.ui.search, visible, url)
            })
        });
    }

    /// The half-page movement step: half the terminal window's rows.
    fn half_page(&self) -> usize {
        self.program
            .window_cells()
            .map_or(10, |s| usize::from(s.height / 2).max(1))
    }

    /// `y`: copy the selected row's link. A no-op without a selection or data.
    fn copy_selected(&mut self) -> Result<()> {
        match self.selected_url() {
            Some(url) => self.copy(&url, 1),
            None => Ok(()),
        }
    }

    /// `Y`: copy every link of the section the cursor is in, as a markdown list.
    /// With no selection that's the first non-empty section, matching where a
    /// movement key would enter. Honors the active search filter, like `targets`.
    fn copy_section(&mut self) -> Result<()> {
        let Some(good) = &self.last_good else {
            return Ok(());
        };
        let mut filtered = None;
        let shown = self.ui.shown(good, &mut filtered);
        let visible = self.visible_sections(shown);
        let urls = nav::section_at_visible(
            self.ui.view,
            shown,
            &self.ui.search,
            self.ui.selected.unwrap_or_default(),
            visible,
        );
        if urls.is_empty() {
            return Ok(());
        }
        let n = urls.len();
        let list = urls
            .iter()
            .map(|u| format!("- {u}"))
            .collect::<Vec<_>>()
            .join("\n");
        self.copy(&list, n)
    }

    /// Hand `text` (`n` links) to the terminal's clipboard and report it on the
    /// trailing status line, which the next refresh clears.
    fn copy(&mut self, text: &str, n: usize) -> Result<()> {
        let plural = if n == 1 { "" } else { "s" };
        self.last_status = match clipboard::copy(text) {
            Ok(()) => format!("copied {n} link{plural}"),
            Err(e) => format!("error: copy failed: {e}"),
        };
        self.ui.selected = None;
        self.repaint_last()
    }

    /// Open the selected row's URL in the browser. A failure becomes the dim
    /// error line; a no-op (no selection, no data) leaves the screen as is.
    fn open_selected(&mut self) -> Result<()> {
        let Some(url) = self.selected_url() else {
            return Ok(());
        };
        if let Err(e) = open::url(&url) {
            self.last_status = format!("error: open failed: {e}");
            self.ui.selected = None;
            self.repaint_last()?;
        }
        Ok(())
    }

    /// Render a successful fetch: diff against the previous snapshot, paint, ring
    /// the bell on a change (once armed), and cache the result.
    fn apply(&mut self, sections: Sections) -> Result<()> {
        let selected = self.selected_url();
        let tracker = Tracker::build(sections.prs.as_deref(), sections.merged.as_deref());
        let changes = self
            .prev
            .as_ref()
            .map(|p| tracker.diff(p))
            .unwrap_or_default();
        let bell = changes.any();

        self.last_status.clear();
        self.prev = Some(tracker);
        self.last_good = Some(sections);
        // Responsive queue membership can change as checks start or finish.
        // Keep the same URL selected instead of reusing its old numeric index.
        self.restore_selection(selected.as_deref());
        self.enter_alt()?;
        self.redraw(&changes)?;

        if self.armed && bell && !self.cli.no_bell {
            let _ = self.program.beep();
        }
        self.armed = true;
        if !self.cli.no_cache
            && let Some(good) = &self.last_good
        {
            cache::save(self.repo, self.cli.required, good);
        }
        Ok(())
    }

    /// Render a failed fetch: keep the last good data, add a dim error line, and
    /// do not ring. With no data yet, just the error line and footer show.
    fn show_error(&mut self, e: anyhow::Error) -> Result<()> {
        self.last_status = format!("error: {}", short_error(&e));
        self.enter_alt()?;
        self.ui.selected = None;
        self.redraw(&Changes::default())
    }

    /// Repaint the current frame in place (after a `?` toggle or a resize), once
    /// there is something to show.
    fn repaint_last(&mut self) -> Result<()> {
        if self.last_good.is_some() {
            self.redraw(&Changes::default())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uncurses::buffer::Surface;
    use uncurses::text::Encode;

    /// Paint a dashboard onto an offscreen buffer and read it back as plain text.
    fn body(sections: &Sections, ui: &Ui) -> String {
        let mut canvas = TextBuffer::new(render::OUTPUT_WIDTH as u16, 64);
        let (used, _, required_width) = paint_dashboard(
            &mut canvas,
            sections,
            ui,
            &Changes::default(),
            "",
            None,
            true,
        );
        assert!(required_width <= render::OUTPUT_WIDTH as u16);
        canvas.resize(render::OUTPUT_WIDTH as u16, used.max(1));
        canvas.display_with(Profile::Disabled).to_string()
    }

    #[test]
    fn staging_buffers_inherit_the_terminal_width_policy() {
        use uncurses::text::WidthMode;

        let screen = TextBuffer::new(1, 1)
            .with_width_mode(WidthMode::Grapheme)
            .with_eaw_wide(true);
        let staged = staging_buffer(&screen, 10, 2);

        assert_eq!(staged.width_mode(), WidthMode::Grapheme);
        assert!(staged.eaw_wide());
    }

    #[test]
    fn fullscreen_resize_repaints_even_at_the_same_size() {
        // Pins the uncurses contract this code leans on: an explicit resize
        // re-establishes the area whatever the size, because a font or window
        // change can move rendered columns while the cell grid stays put.
        let mut screen = Screen::new(Vec::new(), (10, 2));
        screen.set_fullscreen(true);
        screen.set_str((0, 0), "old", None);
        screen.render().unwrap();
        let first = screen.writer().len();

        screen.resize((10, 2));
        screen.render().unwrap();

        assert!(screen.writer().len() > first);
        let output = String::from_utf8_lossy(screen.writer());
        assert_eq!(output.matches("old").count(), 2);
    }

    #[test]
    fn disabled_profiles_force_ascii_content() {
        assert!(ascii_mode(false, Profile::Disabled));
        assert!(ascii_mode(true, Profile::TrueColor));
        assert!(!ascii_mode(false, Profile::TrueColor));
    }

    #[test]
    fn ctrl_c_quits_while_search_is_open() {
        use uncurses::event::Key;

        let ctrl_c = Event::KeyPress(Key::new(KeyCode::Char('c'), KeyModifiers::CTRL));
        let q = Event::KeyPress(Key::new(KeyCode::Char('q'), KeyModifiers::empty()));

        assert_eq!(classify_search(&ctrl_c), SearchAction::Quit);
        assert_eq!(classify_search(&q), SearchAction::Char('q'));
    }

    /// A `Ui` for the given view with nothing selected and no filter.
    fn ui(view: View) -> Ui {
        Ui {
            view,
            show_help: false,
            selected: None,
            search: String::new(),
            searching: false,
            branch: false,
        }
    }

    fn queue_row(n: i64, mine: bool, building: bool) -> queue::QueueRow {
        queue::QueueRow {
            position: n,
            number: n,
            author: if mine { "me" } else { "other" }.into(),
            title: format!("queue-{n}"),
            branch: String::new(),
            url: format!("https://queue/{n}"),
            mine,
            enqueued_at: None,
            build_started_at: None,
            checks: status::Checks {
                running: u64::from(building),
                ..status::Checks::default()
            },
        }
    }

    fn queued_open_row(n: i64) -> prs::PrRow {
        prs::PrRow {
            number: n,
            is_draft: false,
            title: format!("open-queued-{n}"),
            branch: String::new(),
            approval: status::Approval::Approved,
            conflicts: false,
            status: None,
            checks: status::Checks::default(),
            unresolved: 0,
            unresolved_capped: false,
            queue: Some((n, "QUEUED".into())),
            url: format!("https://open/{n}"),
            updated_at: None,
        }
    }

    fn merged_row(n: i64) -> merged::MergedRow {
        merged::MergedRow {
            number: n,
            title: format!("merged-{n}"),
            branch: String::new(),
            url: format!("https://merged/{n}"),
            release: None,
            merged_at: None,
        }
    }

    fn visible_body(sections: &Sections, visible: Visibility) -> String {
        let mut canvas = TextBuffer::new(render::OUTPUT_WIDTH as u16, 64);
        let (used, _, _, required_width) = paint_body(
            &mut canvas,
            sections,
            &ui(View::Mine),
            &Changes::default(),
            true,
            true,
            visible,
            0,
        );
        assert!(required_width <= render::OUTPUT_WIDTH as u16);
        canvas.resize(render::OUTPUT_WIDTH as u16, used.max(1));
        canvas.display_with(Profile::Disabled).to_string()
    }

    fn mine_layout(sections: &Sections, rows: u16) -> ResponsiveLayout {
        responsive_layout(
            120,
            rows,
            sections,
            &ui(View::Mine),
            "",
            Some(("5m", false)),
            true,
            true,
        )
    }

    #[test]
    fn empty_sections_still_show_their_headers_then_a_placeholder() {
        let sections = Sections {
            prs: Some(vec![]),
            queue: Some(vec![]),
            merged: Some(vec![]),
            ..Sections::EMPTY
        };
        let body = body(&sections, &ui(View::Mine));

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

        // ...indented to the row gutter, so it lines up with a section's rows.
        for msg in ["No open PRs.", "No merge queue.", "No recent merged PRs."] {
            let line = body
                .lines()
                .find(|l| l.contains(msg))
                .expect("placeholder line");
            assert_eq!(
                line,
                format!("{}{msg}", " ".repeat(render::ROW_INDENT as usize))
            );
        }
    }

    #[test]
    fn queue_header_shows_next_eta() {
        let sections = Sections {
            queue: Some(vec![]),
            queue_next_eta: Some(11 * 60),
            ..Sections::EMPTY
        };
        let body = body(&sections, &ui(View::Mine));
        assert!(body.contains("Merge Queue (0)"));
        assert!(body.contains("~11m to merge"));
    }

    #[test]
    fn reviews_view_renders_its_own_sections() {
        let sections = Sections {
            reviews: Some(vec![]),
            reviewed_merged: Some(vec![]),
            ..Sections::EMPTY
        };
        let body = body(&sections, &ui(View::Reviews));
        // The Reviews view shows its two headers (not the Mine ones).
        assert!(body.contains("Reviews (0)"));
        assert!(body.contains("Reviewed & merged (0)"));
        assert!(!body.contains("My open PRs"));
    }

    #[test]
    fn responsive_height_hides_help_before_sections() {
        let sections = Sections {
            prs: Some(vec![]),
            queue: Some(vec![]),
            merged: Some(vec![]),
            commits: Some(commits::CommitStats::unavailable()),
            ..Sections::EMPTY
        };
        let ui = Ui {
            show_help: true,
            ..ui(View::Mine)
        };
        // All four sections plus tabs and footer need 14 rows without help.
        let layout =
            responsive_layout(120, 14, &sections, &ui, "", Some(("5m", false)), true, true);
        assert!(!layout.show_help);
        assert_eq!(layout.visible, Visibility::all(&sections));
        assert!(layout.constrained);
        assert!(!layout.too_small);
    }

    #[test]
    fn responsive_height_hides_low_priority_sections_in_order() {
        let sections = Sections {
            prs: Some(vec![]),
            queue: Some(vec![]),
            merged: Some(vec![]),
            commits: Some(commits::CommitStats::unavailable()),
            ..Sections::EMPTY
        };
        // After shipments, merged PRs disappear while the merge queue remains.
        let layout = responsive_layout(
            120,
            9,
            &sections,
            &ui(View::Mine),
            "",
            Some(("5m", false)),
            true,
            true,
        );
        assert_eq!(
            layout.visible,
            Visibility {
                prs: true,
                queue: Some(queue::VisibleRows::All),
                merged: None,
                shipments: false,
                reviews: false,
                reviewed_merged: false,
            }
        );
        assert!(layout.constrained);
        assert!(!layout.too_small);

        // Tabs + the empty open-PR section + footer fit exactly.
        let layout = responsive_layout(
            120,
            6,
            &sections,
            &ui(View::Mine),
            "",
            Some(("5m", false)),
            true,
            true,
        );
        assert_eq!(
            layout.visible,
            Visibility {
                prs: true,
                queue: None,
                merged: None,
                shipments: false,
                reviews: false,
                reviewed_merged: false,
            }
        );
        assert!(layout.constrained);
        assert!(!layout.too_small);
    }

    #[test]
    fn responsive_height_trims_merged_rows_to_one_before_hiding_them() {
        let sections = Sections {
            prs: Some(vec![]),
            merged: Some((1..=5).map(merged_row).collect()),
            ..Sections::EMPTY
        };

        let three = mine_layout(&sections, 13);
        assert_eq!(three.visible.merged, Some(3));

        let one = mine_layout(&sections, 11);
        assert_eq!(one.visible.merged, Some(1));
        let body = visible_body(&sections, one.visible);
        assert!(body.contains("merged-1"), "newest merged PR should remain");
        assert!(
            !body.contains("merged-2"),
            "older merged PR should be hidden"
        );
        assert!(body.contains("+4 hidden"), "hidden count should be shown");

        let none = mine_layout(&sections, 10);
        assert_eq!(none.visible.merged, None);
    }

    #[test]
    fn responsive_height_prioritizes_building_and_mine_queue_rows() {
        let sections = Sections {
            prs: Some(vec![]),
            queue: Some(vec![
                queue_row(1, false, true),
                queue_row(2, true, false),
                queue_row(3, true, true),
                queue_row(4, false, false),
                queue_row(5, false, false),
            ]),
            ..Sections::EMPTY
        };

        let mine_and_building = mine_layout(&sections, 13);
        assert_eq!(
            mine_and_building.visible.queue,
            Some(queue::VisibleRows::BuildingAndMine)
        );
        let body = visible_body(&sections, mine_and_building.visible);
        for title in ["queue-1", "queue-2", "queue-3"] {
            assert!(body.contains(title), "{title} should be visible");
        }
        for title in ["queue-4", "queue-5"] {
            assert!(!body.contains(title), "{title} should be hidden");
        }
        assert!(body.contains("+2 hidden"), "hidden count should be shown");

        let building = mine_layout(&sections, 12);
        assert_eq!(building.visible.queue, Some(queue::VisibleRows::Building));
        let body = visible_body(&sections, building.visible);
        for title in ["queue-1", "queue-3"] {
            assert!(body.contains(title), "{title} should be visible");
        }
        assert!(
            !body.contains("queue-2"),
            "non-building own PR should be hidden"
        );
        assert!(body.contains("+3 hidden"), "hidden count should be shown");

        let none = mine_layout(&sections, 11);
        assert_eq!(none.visible.queue, None);
    }

    #[test]
    fn responsive_height_keeps_marker_only_queue_before_hiding_it() {
        let sections = Sections {
            prs: Some((1..=4).map(queued_open_row).collect()),
            queue: Some((1..=4).map(|n| queue_row(n, true, false)).collect()),
            ..Sections::EMPTY
        };
        let layout = mine_layout(&sections, 9);

        assert_eq!(layout.visible.queue, Some(queue::VisibleRows::Building));
        assert!(!layout.too_small);
        let body = visible_body(&sections, layout.visible);
        assert!(body.contains("Merge Queue (4)"));
        assert!(body.contains("+4 hidden"));
        assert!(!body.contains("No merge queue."));
        assert!(!body.contains("open-queued-"));
        assert!(
            nav::targets_visible(View::Mine, &sections, "", layout.visible).is_empty(),
            "the hidden-count row must not be navigable or copyable"
        );
    }

    #[test]
    fn responsive_height_protects_open_prs_or_reports_too_small() {
        let sections = Sections {
            prs: Some(vec![]),
            queue: Some(vec![]),
            merged: Some(vec![]),
            commits: Some(commits::CommitStats::unavailable()),
            ..Sections::EMPTY
        };
        let layout = responsive_layout(
            120,
            5,
            &sections,
            &ui(View::Mine),
            "",
            Some(("5m", false)),
            true,
            true,
        );
        assert!(layout.too_small);
        assert_eq!(layout.visible, Visibility::none());
        assert_eq!(layout.required_height, 6);
    }

    #[test]
    fn hiding_queue_restores_queued_pr_to_protected_open_section() {
        let sections = Sections {
            prs: Some(vec![prs::PrRow {
                number: 1,
                is_draft: false,
                title: "queued".into(),
                branch: "feature/queued".into(),
                approval: status::Approval::Approved,
                conflicts: false,
                status: None,
                checks: status::Checks::default(),
                unresolved: 0,
                unresolved_capped: false,
                queue: Some((1, "QUEUED".into())),
                url: "https://pr/1".into(),
                updated_at: None,
            }]),
            queue: Some(vec![]),
            ..Sections::EMPTY
        };
        let layout = responsive_layout(
            120,
            7,
            &sections,
            &ui(View::Mine),
            "",
            Some(("5m", false)),
            true,
            true,
        );
        assert!(layout.visible.prs);
        assert!(layout.visible.queue.is_none());
        assert!(!layout.too_small);
        assert_eq!(body_height(&sections, View::Mine, layout.visible, true), 6);
    }

    #[test]
    fn responsive_reviews_protect_the_open_section() {
        let sections = Sections {
            reviews: Some(vec![]),
            reviewed_merged: Some(vec![]),
            ..Sections::EMPTY
        };
        let layout = responsive_layout(
            120,
            6,
            &sections,
            &ui(View::Reviews),
            "",
            Some(("5m", false)),
            true,
            true,
        );
        assert!(layout.visible.reviews);
        assert!(!layout.visible.reviewed_merged);
        assert!(!layout.too_small);
    }

    #[test]
    fn responsive_width_reports_too_small_below_minimum() {
        let layout = responsive_layout(
            render::MIN_WIDTH - 1,
            40,
            &Sections::EMPTY,
            &ui(View::Mine),
            "",
            Some(("5m", false)),
            true,
            true,
        );
        assert!(layout.too_small);
        assert_eq!(layout.required_height, 3);
        assert_eq!(
            too_small_message(render::MIN_WIDTH, layout.required_height),
            "Terminal too small — need 24×3."
        );
    }

    #[test]
    fn selection_highlights_the_chosen_row() {
        let pr = |n: i64| prs::PrRow {
            number: n,
            is_draft: false,
            title: format!("pr {n}"),
            branch: format!("b/{n}"),
            approval: crate::status::Approval::Approved,
            conflicts: false,
            status: None,
            checks: crate::status::Checks::default(),
            unresolved: 0,
            unresolved_capped: false,
            queue: None,
            url: format!("https://pr/{n}"),
            updated_at: None,
        };
        let sections = Sections {
            prs: Some(vec![pr(1), pr(2)]),
            ..Sections::EMPTY
        };

        // The rows carrying the selection background, and their text.
        let highlighted = |ui: &Ui| -> Vec<String> {
            let w = render::OUTPUT_WIDTH as u16;
            let mut canvas = TextBuffer::new(w, 64);
            let (_, _, required_width) = paint_dashboard(
                &mut canvas,
                &sections,
                ui,
                &Changes::default(),
                "",
                None,
                true,
            );
            assert!(required_width <= w);
            let text: Vec<String> = canvas
                .display_with(Profile::Disabled)
                .to_string()
                .lines()
                .map(str::to_string)
                .collect();
            (0..64u16)
                .filter(|&y| {
                    // Edge to edge: the bar covers *every* cell of the row, so
                    // it reads as one solid line rather than stopping at the
                    // text — `all`, not `any`.
                    (0..w).all(|x| {
                        canvas
                            .cell(uncurses::layout::Position::new(x, y))
                            .is_some_and(|c| c.style.bg == Some(crate::status::SURFACE))
                    })
                })
                .map(|y| text.get(y as usize).cloned().unwrap_or_default())
                .collect()
        };

        // No selection -> nothing is highlighted (the glanceable default).
        assert!(highlighted(&ui(View::Mine)).is_empty());

        // Selecting the second row highlights exactly that row, whole: the bar
        // reaches the leading marker column, which the caret used to occupy.
        let sel = highlighted(&Ui {
            selected: Some(1),
            ..ui(View::Mine)
        });
        assert_eq!(sel.len(), 1, "expected one highlighted row, got {sel:?}");
        assert!(sel[0].contains("#2"), "wrong row highlighted: {:?}", sel[0]);
    }
}

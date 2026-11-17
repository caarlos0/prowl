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
mod config;
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
#[cfg(test)]
use clap::Parser;
use cli::{Cli, View};
use github::{Client, Repo};
use std::borrow::Cow;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use uncurses::ansi::kitty::KittyKeyboardFlags;
use uncurses::buffer::{Bounded, SurfaceMut, TextBuffer};
use uncurses::color::{Color, Profile};
use uncurses::event::{Event, MouseButton};
#[cfg(test)]
use uncurses::event::{KeyCode, KeyModifiers};
use uncurses::layout::Position;
use uncurses::program::{MouseTracking, Program};
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
    /// Every section disabled — only status, footer, and optional help remain
    /// when a fetch fails before any data arrives.
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
enum FetchScope {
    Mine,
    Queue,
    Merged,
    Upcoming,
    Shipments,
    ReleaseMetadata,
    Reviews,
    ReviewedMerged,
}

impl FetchScope {
    fn label(self) -> &'static str {
        match self {
            Self::Mine => "My PRs",
            Self::Queue => "Merge Queue",
            Self::Merged => "My Merged PRs",
            Self::Upcoming => "My Shipments upcoming",
            Self::Shipments => "My Shipments",
            Self::ReleaseMetadata => "Merged release metadata",
            Self::Reviews => "Reviews",
            Self::ReviewedMerged => "Reviewed & merged",
        }
    }

    fn view(self) -> View {
        match self {
            Self::Mine
            | Self::Queue
            | Self::Merged
            | Self::Upcoming
            | Self::Shipments
            | Self::ReleaseMetadata => View::Mine,
            Self::Reviews | Self::ReviewedMerged => View::Reviews,
        }
    }
}

#[derive(Debug)]
struct FetchIssue {
    scope: FetchScope,
    message: String,
    stale: bool,
    warning: bool,
}

impl FetchIssue {
    fn new(scope: FetchScope, error: &anyhow::Error) -> Self {
        Self {
            scope,
            message: short_error(error).replace(['\n', '\r'], " "),
            stale: false,
            warning: false,
        }
    }

    fn partial(scope: FetchScope, errors: &[github::GqlError]) -> Self {
        let mut messages = Vec::new();
        for error in errors {
            let message = error.message.replace(['\n', '\r'], " ");
            if !messages.contains(&message) {
                messages.push(message);
            }
        }
        Self {
            scope,
            message: messages.join("; "),
            stale: false,
            warning: true,
        }
    }

    fn line(&self) -> String {
        if self.warning {
            return format!(
                "warning: {} partial data: {}",
                self.scope.label(),
                self.message
            );
        }
        format!(
            "error: {} {}: {}",
            self.scope.label(),
            if self.stale { "stale" } else { "unavailable" },
            self.message
        )
    }
}

struct FetchOutcome {
    sections: Sections,
    issues: Vec<FetchIssue>,
}

#[derive(Default)]
struct FetchedReviewSections {
    merged_nodes: Option<Vec<model::MergedNode>>,
    reviews: Option<Vec<reviews::ReviewRow>>,
    reviewed_merged: Option<Vec<reviews::ReviewedMergedRow>>,
}

impl FetchOutcome {
    fn status(&self) -> String {
        self.issues
            .iter()
            .map(FetchIssue::line)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn warnings(&self) -> Vec<FetchScope> {
        self.issues
            .iter()
            .filter(|issue| issue.warning)
            .map(|issue| issue.scope)
            .collect()
    }

    fn has_errors(&self) -> bool {
        self.issues.iter().any(|issue| !issue.warning)
    }

    fn reconcile(mut self, previous: Option<Sections>) -> Self {
        let mut previous = previous.unwrap_or(Sections::EMPTY);
        for issue in &mut self.issues {
            if issue.warning {
                continue;
            }
            match issue.scope {
                FetchScope::Mine => {
                    retain(&mut self.sections.prs, &mut previous.prs, issue);
                }
                FetchScope::Queue => {
                    retain(&mut self.sections.queue, &mut previous.queue, issue);
                    if issue.stale {
                        self.sections.queue_next_eta = previous.queue_next_eta.take();
                    }
                }
                FetchScope::Merged => {
                    retain(&mut self.sections.merged, &mut previous.merged, issue);
                }
                FetchScope::Shipments => {
                    if self
                        .sections
                        .commits
                        .as_ref()
                        .is_some_and(|stats| stats.available)
                    {
                        issue.stale = true;
                    } else if previous
                        .commits
                        .as_ref()
                        .is_some_and(|stats| stats.available)
                    {
                        issue.stale = true;
                        self.sections.commits = previous.commits.take();
                    }
                }
                FetchScope::Upcoming => {
                    if let Some(stats) = self.sections.commits.as_mut() {
                        stats.upcoming = None;
                    } else if let Some(mut stats) = previous.commits.take()
                        && stats.available
                    {
                        stats.upcoming = None;
                        self.sections.commits = Some(stats);
                    }
                }
                FetchScope::ReleaseMetadata => {
                    let releases: std::collections::HashMap<_, _> = previous
                        .merged
                        .iter()
                        .flatten()
                        .filter_map(|row| row.release.as_ref().map(|release| (row.number, release)))
                        .collect();
                    issue.stale |= !releases.is_empty();
                    if let Some(rows) = &mut self.sections.merged {
                        for row in rows {
                            if row.release.is_none() {
                                row.release =
                                    releases.get(&row.number).map(|release| (*release).clone());
                                row.release_unknown = row.release.is_none();
                            }
                        }
                    }
                }
                FetchScope::Reviews => {
                    retain(&mut self.sections.reviews, &mut previous.reviews, issue);
                }
                FetchScope::ReviewedMerged => {
                    retain(
                        &mut self.sections.reviewed_merged,
                        &mut previous.reviewed_merged,
                        issue,
                    );
                }
            }
        }
        self
    }

    /// Combine a watch refresh with the previous data. The sections and issues
    /// of views this refresh skipped carry over unchanged; fetched sections are
    /// reconciled as usual.
    fn merge(
        mut self,
        previous: Option<Sections>,
        previous_issues: Vec<FetchIssue>,
        due: Due,
    ) -> Self {
        let mut previous = previous.unwrap_or(Sections::EMPTY);
        if !due.mine {
            self.sections.merged = previous.merged.take();
            self.sections.queue = previous.queue.take();
            self.sections.queue_next_eta = previous.queue_next_eta.take();
            self.sections.prs = previous.prs.take();
            self.sections.commits = previous.commits.take();
        }
        if !due.reviews {
            self.sections.reviews = previous.reviews.take();
            self.sections.reviewed_merged = previous.reviewed_merged.take();
        }
        let mut merged = self.reconcile(Some(previous));
        merged.issues.extend(
            previous_issues
                .into_iter()
                .filter(|issue| !due.includes(issue.scope.view())),
        );
        // Keep the fetch order (Mine before Reviews) on every refresh.
        merged
            .issues
            .sort_by_key(|issue| issue.scope.view() == View::Reviews);
        merged
    }
}

/// When a view was last fetched: the refresh round and the wall-clock time.
#[derive(Clone, Copy)]
struct Fetched {
    refresh: u64,
    at: chrono::DateTime<chrono::Utc>,
}

/// The views one watch refresh fetches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Due {
    mine: bool,
    reviews: bool,
}

impl Due {
    fn includes(self, view: View) -> bool {
        match view {
            View::Mine => self.mine,
            View::Reviews => self.reviews,
        }
    }
}

/// The watch refresh cadence. The active view is fetched on every refresh, the
/// inactive view on every [`Self::INACTIVE_EVERY`]th, and `Ctrl-R` fetches both.
/// A view that was never fetched is always due.
#[derive(Default)]
struct Schedule {
    refresh: u64,
    mine: Option<Fetched>,
    reviews: Option<Fetched>,
}

impl Schedule {
    const INACTIVE_EVERY: u64 = 2;

    fn last(&self, view: View) -> Option<Fetched> {
        match view {
            View::Mine => self.mine,
            View::Reviews => self.reviews,
        }
    }

    /// Start the next refresh and select the views it fetches.
    fn next(&mut self, active: View, manual: bool) -> Due {
        self.refresh += 1;
        let due = |view| {
            view == active
                || manual
                || self
                    .last(view)
                    .is_none_or(|last| self.refresh - last.refresh >= Self::INACTIVE_EVERY)
        };
        Due {
            mine: due(View::Mine),
            reviews: due(View::Reviews),
        }
    }

    /// Record a completed refresh of the `due` views. A view with a failed
    /// section keeps its previous time, so the next refresh retries it. A
    /// partial-data warning still delivered data, so it counts as fetched.
    fn record(&mut self, due: Due, issues: &[FetchIssue], at: chrono::DateTime<chrono::Utc>) {
        let fetched = Some(Fetched {
            refresh: self.refresh,
            at,
        });
        let succeeded = |view| {
            due.includes(view) && !issues.iter().any(|i| !i.warning && i.scope.view() == view)
        };
        if succeeded(View::Mine) {
            self.mine = fetched;
        }
        if succeeded(View::Reviews) {
            self.reviews = fetched;
        }
    }

    /// The age of `view`'s data, when it is older than the other view's.
    fn age_note(&self, view: View, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
        let shown = self.last(view)?;
        let other = self.last(view.toggle())?;
        if shown.refresh >= other.refresh {
            return None;
        }
        let age = timefmt::relative_age(now, shown.at);
        Some(if age == "now" {
            "updated just now".to_string()
        } else {
            format!("updated {age} ago")
        })
    }
}

fn retain<T>(fresh: &mut Option<T>, previous: &mut Option<T>, issue: &mut FetchIssue) {
    if let Some(data) = previous.take() {
        *fresh = Some(data);
        issue.stale = true;
    }
}

fn section<T>(scope: FetchScope, result: Result<T>, issues: &mut Vec<FetchIssue>) -> Option<T> {
    match result {
        Ok(data) => Some(data),
        Err(error) => {
            issues.push(FetchIssue::new(scope, &error));
            None
        }
    }
}

fn graphql_section<T, U>(
    scope: FetchScope,
    result: Result<model::GraphqlData<T>>,
    map: impl FnOnce(T) -> U,
    issues: &mut Vec<FetchIssue>,
) -> Option<U> {
    match result {
        Ok(response) => {
            if !response.errors.is_empty() {
                issues.push(FetchIssue::partial(scope, &response.errors));
            }
            Some(map(response.data))
        }
        Err(error) => {
            issues.push(FetchIssue::new(scope, &error));
            None
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Visibility {
    pub(crate) prs: bool,
    pub(crate) queue: bool,
    pub(crate) merged: bool,
    pub(crate) shipments: bool,
    pub(crate) reviews: bool,
    pub(crate) reviewed_merged: bool,
}

impl Visibility {
    pub(crate) fn all(sections: &Sections) -> Self {
        Self {
            prs: sections.prs.is_some(),
            queue: sections.queue.is_some(),
            merged: sections.merged.is_some(),
            shipments: sections.commits.is_some(),
            reviews: sections.reviews.is_some(),
            reviewed_merged: sections.reviewed_merged.is_some(),
        }
    }
}

fn bottom_height(ui: &Ui, status: &str, footer: Option<(&str, bool)>, show_help: bool) -> usize {
    let search = usize::from(!ui.search.is_empty() || ui.searching);
    let footer = usize::from(footer.is_some());
    let blocks = search + usize::from(!status.is_empty()) + footer;
    let help = if show_help {
        render::help_height(ui.view)
    } else {
        0
    };
    let content_lines = search + status.lines().count() + footer;
    let gaps = (blocks + usize::from(show_help)).saturating_sub(1);
    content_lines + help + gaps
}

fn fetch_merged_release_map(
    cli: &Cli,
    client: &Client,
    repo: &Repo,
    me: &str,
    nodes: &[model::MergedNode],
    issues: &mut Vec<FetchIssue>,
) -> commits::ReleaseMap {
    let prs: Vec<_> = nodes
        .iter()
        .take(cli.merged_limit)
        .map(|node| node.number)
        .collect();
    match commits::fetch_release_map(client, repo, me, cli.include_pre_releases, &prs) {
        Ok((map, errors)) => {
            release_errors(errors, issues, FetchScope::ReleaseMetadata, false);
            map
        }
        Err(error) => {
            issues.push(FetchIssue::new(FetchScope::ReleaseMetadata, &error));
            commits::ReleaseMap::incomplete()
        }
    }
}

/// Fetch the sections for the requested views. `want_mine` covers the Mine view
/// (open PRs, queue, merged, shipments, honoring `--only`); `want_reviews`
/// covers the Reviews view (PRs to review, reviewed-and-merged). The watch asks
/// for the views its [`Schedule`] says are due; `--once` fetches just one.
fn fetch(
    cli: &Cli,
    client: &Client,
    repo: &Repo,
    me: &str,
    default_branch: &mut Option<String>,
    want_mine: bool,
    want_reviews: bool,
) -> FetchOutcome {
    let mut issues = Vec::new();
    let branch_error = if want_mine && cli.show_shipments() {
        resolve_default_branch(client, repo, default_branch)
    } else {
        None
    };
    let default_branch = if want_mine && cli.show_shipments() {
        default_branch.as_deref()
    } else {
        None
    };

    let (commit_stats, mut release_map) = fetch_release_data(
        cli,
        client,
        repo,
        me,
        default_branch,
        want_mine,
        &mut issues,
    );
    if let Some(error) = branch_error {
        issues.push(FetchIssue::new(FetchScope::Upcoming, &error));
    }

    let fetched_reviews =
        fetch_review_sections(cli, client, repo, me, want_mine, want_reviews, &mut issues);
    let merged_nodes = fetched_reviews.merged_nodes;
    if !cli.show_shipments()
        && let Some(nodes) = merged_nodes.as_deref()
    {
        release_map = fetch_merged_release_map(cli, client, repo, me, nodes, &mut issues);
    }
    let merged =
        merged_nodes.map(|nodes| merged::build_rows(nodes, cli.merged_limit, &release_map));
    let mut required_context_cache = model::RequiredContextCache::default();
    let (queue, queue_next_eta) = if want_mine && cli.show_queue() {
        graphql_section(
            FetchScope::Queue,
            model::fetch_queue_with_context_cache(
                client,
                repo,
                cli.required,
                &mut required_context_cache,
            ),
            |(nodes, eta)| (queue::build_rows(nodes, me), eta),
            &mut issues,
        )
        .map_or((None, None), |(rows, eta)| (Some(rows), eta))
    } else {
        (None, None)
    };
    let prs = if want_mine && cli.show_mine() {
        graphql_section(
            FetchScope::Mine,
            model::fetch_my_prs_with_context_cache(
                client,
                repo,
                me,
                cli.sort_open,
                cli.required,
                &mut required_context_cache,
            ),
            |nodes| {
                let rows = prs::build_rows(nodes, cli.sort_open);
                if cli.no_draft {
                    prs::without_drafts(rows)
                } else {
                    rows
                }
            },
            &mut issues,
        )
    } else {
        None
    };
    let commits = if want_mine && cli.show_shipments() {
        commit_stats
    } else {
        None
    };

    FetchOutcome {
        sections: Sections {
            merged,
            queue,
            queue_next_eta,
            prs,
            commits,
            reviews: fetched_reviews.reviews,
            reviewed_merged: fetched_reviews.reviewed_merged,
        },
        issues,
    }
}

fn resolve_default_branch(
    client: &Client,
    repo: &Repo,
    branch: &mut Option<String>,
) -> Option<anyhow::Error> {
    if branch.is_some() {
        return None;
    }
    match client.default_branch(repo) {
        Ok(resolved) => {
            *branch = Some(resolved);
            None
        }
        Err(error) => Some(error),
    }
}

fn fetch_release_data(
    cli: &Cli,
    client: &Client,
    repo: &Repo,
    me: &str,
    default_branch: Option<&str>,
    want_mine: bool,
    issues: &mut Vec<FetchIssue>,
) -> (Option<commits::CommitStats>, commits::ReleaseMap) {
    // Shipment counts also supply merged-row release annotations.
    if want_mine && cli.show_shipments() {
        match commits::fetch(client, repo, me, default_branch, cli.include_pre_releases) {
            Ok((stats, release_map, errors)) => {
                release_errors(errors, issues, FetchScope::Shipments, cli.show_merged());
                (Some(stats), release_map)
            }
            Err(error) => {
                issues.push(FetchIssue::new(FetchScope::Shipments, &error));
                if cli.show_merged() {
                    issues.push(FetchIssue::new(FetchScope::ReleaseMetadata, &error));
                }
                (
                    Some(commits::CommitStats::unavailable()),
                    commits::ReleaseMap::incomplete(),
                )
            }
        }
    } else {
        (None, commits::ReleaseMap::new())
    }
}

fn release_errors(
    errors: Vec<commits::FetchError>,
    issues: &mut Vec<FetchIssue>,
    scope: FetchScope,
    report_metadata: bool,
) {
    let mut metadata_issue = None;
    for error in errors {
        match error {
            commits::FetchError::Upcoming(error) => {
                issues.push(FetchIssue::new(FetchScope::Upcoming, &error));
            }
            commits::FetchError::Release { tag, error } => {
                let mut issue = FetchIssue::new(scope, &error);
                issue.message = format!("{tag}: {}", issue.message);
                issue.stale = true;
                if report_metadata {
                    metadata_issue.get_or_insert_with(|| {
                        let mut issue = FetchIssue::new(FetchScope::ReleaseMetadata, &error);
                        issue.message = format!("{tag}: {}", issue.message);
                        issue.stale = true;
                        issue
                    });
                }
                issues.push(issue);
            }
        }
    }
    if let Some(issue) = metadata_issue {
        issues.push(issue);
    }
}

fn fetch_review_sections(
    cli: &Cli,
    client: &Client,
    repo: &Repo,
    me: &str,
    want_mine: bool,
    want_reviews: bool,
    issues: &mut Vec<FetchIssue>,
) -> FetchedReviewSections {
    if !want_reviews {
        let merged_nodes = if want_mine && cli.show_merged() {
            let since = timefmt::since_date(&cli.merged_window);
            graphql_section(
                FetchScope::Merged,
                model::fetch_merged_with_errors(client, repo, me, &since, cli.merged_limit),
                |nodes| nodes,
                issues,
            )
        } else {
            None
        };
        return FetchedReviewSections {
            merged_nodes,
            ..FetchedReviewSections::default()
        };
    }

    let include_mine = want_mine && cli.show_merged();
    let since = timefmt::since_date(&cli.merged_window);
    match model::fetch_reviews_and_merged(
        client,
        repo,
        me,
        cli.review_scope.qualifier(),
        &since,
        cli.merged_limit,
        include_mine,
    ) {
        Ok(fetched) => {
            let merged_nodes = fetched.mine_merged.and_then(|result| {
                graphql_section(FetchScope::Merged, result, |nodes| nodes, issues)
            });
            let reviews = graphql_section(
                FetchScope::Reviews,
                fetched.reviews,
                |data| {
                    let rows = reviews::build_open_rows(data);
                    if cli.no_draft {
                        reviews::without_drafts(rows)
                    } else {
                        rows
                    }
                },
                issues,
            );
            let reviewed_merged = graphql_section(
                FetchScope::ReviewedMerged,
                fetched.reviewed_merged,
                |nodes| reviews::build_merged_rows(nodes, cli.merged_limit),
                issues,
            );
            FetchedReviewSections {
                merged_nodes,
                reviews,
                reviewed_merged,
            }
        }
        Err(error) => {
            let message = format!("{error:#}");
            let merged_nodes = if include_mine {
                section(
                    FetchScope::Merged,
                    Err(anyhow::anyhow!("{message}")),
                    issues,
                )
            } else {
                None
            };
            let reviews = section(
                FetchScope::Reviews,
                Err(anyhow::anyhow!("{message}")),
                issues,
            );
            let reviewed_merged = section(
                FetchScope::ReviewedMerged,
                Err(anyhow::anyhow!("{message}")),
                issues,
            );
            FetchedReviewSections {
                merged_nodes,
                reviews,
                reviewed_merged,
            }
        }
    }
}

/// Paint one PR section onto `s` at row `top`: a counted header (with an optional
/// dim note), then either its table or, when empty, a dim placeholder.
/// Returns the next free row.
#[allow(clippy::too_many_arguments)]
fn paint_section(
    s: &mut impl TextSurface,
    title: &str,
    accent: Color,
    count: usize,
    note: Option<&str>,
    empty_msg: &str,
    table: Option<&render::Table>,
    alignment: &render::TableAlignment,
    ascii: bool,
    top: u16,
) -> u16 {
    let y = render::paint_header(s, title, accent, Some(&count.to_string()), note, ascii, top);
    let y = match table {
        Some(table) => render::paint_table_aligned(s, table, alignment, ascii, y),
        None => render::paint_dim_at(s, empty_msg, render::ROW_INDENT, y),
    };
    y + 1
}

fn table_state(s: &impl TextSurface, tables: &[&render::Table]) -> (render::TableAlignment, bool) {
    let alignment = render::table_alignment(s, tables);
    let compact = tables
        .iter()
        .any(|table| render::table_is_compact_aligned(s, table, &alignment));
    (alignment, compact)
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
/// Each enabled section shows its header and full count.
/// Empty sections show a dim placeholder. Returns the next free row and the
/// selected row, if any.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn paint_mine(
    s: &mut impl TextSurface,
    sections: &Sections,
    changes: &Changes,
    warnings: &[FetchScope],
    selected: Option<usize>,
    ascii: bool,
    show_branch: bool,
    visible: Visibility,
    top: u16,
    row_positions: &mut Vec<u16>,
) -> (u16, Option<u16>, bool) {
    let prs_rows = sections.prs.as_deref().filter(|_| visible.prs).map(|rows| {
        if visible.queue {
            Cow::Owned(prs::without_queued(rows.to_vec()))
        } else {
            Cow::Borrowed(rows)
        }
    });
    let prs_table = prs_rows
        .as_deref()
        .filter(|r| !r.is_empty())
        .map(|rows| prs::to_table(rows, ascii, &changes.status_changed, show_branch));
    let queue_rows = sections.queue.as_deref().filter(|_| visible.queue);
    let queue_table = queue_rows
        .filter(|r| !r.is_empty())
        .map(|rows| queue::to_table(rows, ascii, show_branch));
    let merged_rows = sections.merged.as_deref().filter(|_| visible.merged);
    let merged_table = merged_rows
        .filter(|r| !r.is_empty())
        .map(|rows| merged::to_table(rows, ascii, &changes.newly_merged, show_branch));
    let tables: Vec<&render::Table> = [&prs_table, &queue_table, &merged_table]
        .into_iter()
        .flatten()
        .collect();
    let (alignment, compact) = table_state(s, &tables);
    let np = prs_rows.as_deref().map_or(0, <[prs::PrRow]>::len);
    let nq = queue_rows.map_or(0, <[queue::QueueRow]>::len);
    let nm = merged_rows.map_or(0, <[merged::MergedRow]>::len);
    let (prs_sel, queue_sel, merged_sel, ship_sel) = mine_selection(selected, np, nq, nm);
    let prs_sel = prs_sel.filter(|&local| selects_row(prs_table.as_ref(), local));
    let queue_sel = queue_sel.filter(|&local| selects_row(queue_table.as_ref(), local));
    let merged_sel = merged_sel.filter(|&local| selects_row(merged_table.as_ref(), local));

    let mut y = top;
    let mut caret = None;
    if let Some(rows) = prs_rows.as_deref() {
        row_positions.extend((0..rows.len()).map(|local| caret_row(y, local)));
        caret = caret.or(prs_sel.map(|l| caret_row(y, l)));
        y = paint_section(
            s,
            "My open PRs",
            status::GREEN,
            rows.len(),
            warnings
                .contains(&FetchScope::Mine)
                .then_some("partial data"),
            "No open PRs.",
            prs_table.as_ref(),
            &alignment,
            ascii,
            y,
        );
    }
    if let Some(rows) = queue_rows {
        row_positions.extend((0..rows.len()).map(|local| caret_row(y, local)));
        // The queue-level ETA (time until a newly added entry would merge) rides
        // alongside the header as a dim note.
        caret = caret.or(queue_sel.map(|l| caret_row(y, l)));
        let eta = sections.queue_next_eta.map(|secs| {
            format!(
                "~{} to merge",
                timefmt::eta(Duration::from_secs(secs.max(0) as u64))
            )
        });
        let warning = warnings.contains(&FetchScope::Queue);
        let note = match (eta.as_deref(), warning) {
            (Some(eta), true) => Some(format!("{eta}; partial data")),
            (Some(eta), false) => Some(eta.to_string()),
            (None, true) => Some("partial data".to_string()),
            (None, false) => None,
        };
        y = paint_section(
            s,
            "Merge Queue",
            status::PEACH,
            rows.len(),
            note.as_deref(),
            "No merge queue.",
            queue_table.as_ref(),
            &alignment,
            ascii,
            y,
        );
    }
    if let Some(rows) = merged_rows {
        row_positions.extend((0..rows.len()).map(|local| caret_row(y, local)));
        caret = caret.or(merged_sel.map(|l| caret_row(y, l)));
        y = paint_section(
            s,
            "My merged PRs",
            status::MAUVE,
            rows.len(),
            warnings
                .contains(&FetchScope::Merged)
                .then_some("partial data"),
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
        let (next, ship_caret) = paint_commits(s, stats, ship_sel, ascii, y, row_positions);
        y = next + 1;
        caret = caret.or(ship_caret);
    }
    (y, caret, compact)
}

/// The Reviews view: PRs to review (with a per-row review-state glyph), then
/// merged PRs I reviewed. Returns the next free row, the selection caret, and
/// whether the width hid information.
#[allow(clippy::too_many_arguments)]
fn paint_reviews(
    s: &mut impl TextSurface,
    sections: &Sections,
    warnings: &[FetchScope],
    selected: Option<usize>,
    ascii: bool,
    show_branch: bool,
    visible: Visibility,
    top: u16,
    row_positions: &mut Vec<u16>,
) -> (u16, Option<u16>, bool) {
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
    let (alignment, compact) = table_state(s, &tables);
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
        row_positions.extend((0..rows.len()).map(|local| caret_row(y, local)));
        caret = open_sel.map(|l| caret_row(y, l));
        y = paint_section(
            s,
            "Reviews",
            status::LAVENDER,
            rows.len(),
            warnings
                .contains(&FetchScope::Reviews)
                .then_some("partial data"),
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
        row_positions.extend((0..rows.len()).map(|local| caret_row(y, local)));
        caret = caret.or(merged_sel.map(|l| caret_row(y, l)));
        y = paint_section(
            s,
            "Reviewed & merged",
            status::MAUVE,
            rows.len(),
            warnings
                .contains(&FetchScope::ReviewedMerged)
                .then_some("partial data"),
            "No reviewed PRs merged recently.",
            merged_table.as_ref(),
            &alignment,
            ascii,
            y,
        );
    }
    (y, caret, compact)
}

/// Paint the dashboard's body onto `s` from row `top`: the watch-only tab strip
/// and the active view's sections. Rows with unread changes
/// (per `changes`) are flagged with a leading marker. `tabs` is set only while
/// watching, since the view switcher is an interactive affordance. `ascii`
/// selects letters/parens over Nerd Font glyphs/bars; colors are written as
/// styles and downsampled by the surface's `Profile` at encode/render time.
///
/// Returns the next free row, the selection caret, and whether width hid information.
#[allow(clippy::too_many_arguments)]
fn paint_body(
    s: &mut impl TextSurface,
    sections: &Sections,
    ui: &Ui,
    changes: &Changes,
    warnings: &[FetchScope],
    ascii: bool,
    tabs: bool,
    visible: Visibility,
    top: u16,
    row_positions: &mut Vec<u16>,
) -> (u16, Option<u16>, bool) {
    let mut y = top;
    if tabs {
        y = render::paint_tabs(s, ui.view, ascii, y) + 1;
    }
    let (y, caret, compact) = match ui.view {
        View::Mine => paint_mine(
            s,
            sections,
            changes,
            warnings,
            ui.selected,
            ascii,
            ui.branch,
            visible,
            y,
            row_positions,
        ),
        View::Reviews => paint_reviews(
            s,
            sections,
            warnings,
            ui.selected,
            ascii,
            ui.branch,
            visible,
            y,
            row_positions,
        ),
    };
    // Highlight the selected row once the whole body is painted, so the bar
    // spans the content and covers the hand-laid-out shipments section too.
    if let Some(row) = caret {
        render::highlight_row(s, row);
    }
    (y, caret, compact)
}

/// Paint the bottom block from row `top`: optional one-shot help, search prompt,
/// status, then footer. A blank row separates each part. The watch pins the
/// search prompt, status, and footer to the last screen rows.
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
        for line in status.lines() {
            y = render::paint_dim(s, line, y);
        }
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

#[derive(Clone, Copy)]
struct RenderMessages<'a> {
    warnings: &'a [FetchScope],
    status: &'a str,
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
    messages: RenderMessages<'_>,
    footer: Option<(&str, bool)>,
    ascii: bool,
) -> (u16, Option<Position>) {
    let visible = Visibility::all(sections);
    let (y, _, compact) = paint_body(
        s,
        sections,
        ui,
        changes,
        messages.warnings,
        ascii,
        footer.is_some(),
        visible,
        0,
        &mut Vec::new(),
    );
    let (used, caret) = paint_bottom(
        s,
        sections,
        ui,
        messages.status,
        footer,
        ascii,
        ui.show_help,
        visible,
        compact,
        y,
    );
    (used, caret)
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
fn bottom_bound(ui: &Ui, status: &str) -> u16 {
    let mut n = 6usize;
    if ui.show_help {
        n += render::help_height(ui.view) + 1;
    }
    n += status.lines().count().saturating_sub(1);
    n as u16
}

fn watch_body(
    screen: &impl TextSurface,
    sections: &Sections,
    ui: &Ui,
    changes: &Changes,
    warnings: &[FetchScope],
    ascii: bool,
) -> (TextBuffer, u16, Option<u16>, bool, Vec<u16>) {
    let mut body = staging_buffer(
        screen,
        screen.bounds().width,
        height_bound(sections, ui).max(1),
    );
    let top = if ui.show_help {
        render::paint_help(&mut body, ui.view, ascii, 0) + 1
    } else {
        0
    };
    let mut row_positions = Vec::new();
    let (body_h, selected, compact) = paint_body(
        &mut body,
        sections,
        ui,
        changes,
        warnings,
        ascii,
        true,
        Visibility::all(sections),
        top,
        &mut row_positions,
    );
    (body, body_h, selected, compact, row_positions)
}

/// Paint a watch frame with a scrollable body and a pinned bottom block.
/// The caller must clear the surface first.
#[allow(clippy::too_many_arguments)]
pub fn paint_watch_frame(
    screen: &mut impl TextSurface,
    sections: &Sections,
    ui: &Ui,
    changes: &Changes,
    status: &str,
    footer: Option<(&str, bool)>,
    ascii: bool,
) -> Option<Position> {
    paint_watch_frame_with_warnings(screen, sections, ui, changes, &[], status, footer, ascii)
}

#[allow(clippy::too_many_arguments)]
fn paint_watch_frame_with_warnings(
    screen: &mut impl TextSurface,
    sections: &Sections,
    ui: &Ui,
    changes: &Changes,
    warnings: &[FetchScope],
    status: &str,
    footer: Option<(&str, bool)>,
    ascii: bool,
) -> Option<Position> {
    let bounds = screen.bounds();
    let visible = Visibility::all(sections);
    let (mut body, body_h, selected, compact, _) =
        watch_body(screen, sections, ui, changes, warnings, ascii);
    let bottom_h = bottom_height(ui, status, footer, false) as u16;
    let mut bottom = staging_buffer(screen, bounds.width, bottom_h.max(1));
    let (bottom_h, caret) = paint_bottom(
        &mut bottom,
        sections,
        ui,
        status,
        footer,
        ascii,
        false,
        visible,
        compact,
        0,
    );
    let offset = ui.scroll.unwrap_or_else(|| {
        render::body_scroll_offset(body_h, bounds.height.saturating_sub(bottom_h), selected)
    });
    let (top, cut) = render::compose_scrolled(
        screen,
        &mut body,
        body_h,
        &mut bottom,
        bottom_h,
        bounds.height,
        offset,
    );
    caret
        .filter(|p| p.y >= cut)
        .map(|p| Position::new(p.x.min(bounds.width.saturating_sub(1)), top + p.y - cut))
}

/// Paint the dashboard onto a `Screen` and render it.
///
/// `pinned` is the watch layout: the screen is the whole terminal, the bottom
/// block (search prompt, error line, footer) is glued to its last rows,
/// and the body scrolls to the selected row. Unpinned
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
    warnings: &[FetchScope],
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
        screen.clear();
        paint_watch_frame_with_warnings(
            screen, sections, ui, changes, warnings, status, footer, ascii,
        )
    } else {
        let w = screen.width().max(1);
        // Inline output keeps its full height, even on a short terminal.
        screen.resize((
            w,
            (height_bound(sections, ui) + bottom_bound(ui, status)).max(1),
        ));
        screen.clear();
        let (used, caret) = paint_dashboard(
            screen,
            sections,
            ui,
            changes,
            RenderMessages { warnings, status },
            footer,
            ascii,
        );
        screen.resize((w, used.max(1)));
        caret
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
    render_to_string_with_status(sections, ui, changes, "", footer, ascii, profile)
}

fn render_to_string_with_status(
    sections: &Sections,
    ui: &Ui,
    changes: &Changes,
    status: &str,
    footer: Option<(&str, bool)>,
    ascii: bool,
    profile: Profile,
) -> String {
    render_to_string_with_warnings(
        sections,
        ui,
        changes,
        RenderMessages {
            warnings: &[],
            status,
        },
        footer,
        ascii,
        profile,
    )
}

fn render_to_string_with_warnings(
    sections: &Sections,
    ui: &Ui,
    changes: &Changes,
    messages: RenderMessages<'_>,
    footer: Option<(&str, bool)>,
    ascii: bool,
    profile: Profile,
) -> String {
    let w = render::OUTPUT_WIDTH as u16;
    let mut canvas = TextBuffer::new(
        w,
        height_bound(sections, ui) + bottom_bound(ui, messages.status),
    );
    // One-shot output never searches, so there is no caret to place.
    let (used, _) = paint_dashboard(&mut canvas, sections, ui, changes, messages, footer, ascii);
    canvas.resize(w, used.max(1));

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
    warnings: &[FetchScope],
    status: &str,
    footer: Option<(&str, bool)>,
) -> Result<()> {
    let profile = Profile::detect_from(terminal.env(), terminal.is_terminal().1);
    let ascii = ascii_mode(cli.ascii, profile);
    // One-shot output has no interaction: no tabs, no selection, no search; the
    // help legend follows `--no-help` instead of the `?` toggle.
    let ui = Ui::once(cli);
    let painted = render_to_string_with_warnings(
        sections,
        &ui,
        changes,
        RenderMessages { warnings, status },
        footer,
        ascii,
        profile,
    );

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
) -> Result<Option<FetchOutcome>> {
    // Inline loading frame; raw mode swallows keystrokes so nothing echoes into
    // the output while we wait.
    paint_loading(program.screen_mut())?;

    // Fetch off-thread so `q` stays live during network I/O. `me` and the
    // default branch (when shipments need it) are resolved here too, so even
    // the first round-trip never blocks the abort key.
    let (cli2, client2, repo2) = (cli.clone(), client.clone(), repo.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let fetched = (|| -> Result<FetchOutcome> {
            let me = client2.me()?;
            // Only the selected view: `--once` output has no Tab to switch.
            Ok(fetch(
                &cli2,
                &client2,
                &repo2,
                &me,
                &mut None,
                cli2.view == View::Mine,
                cli2.view == View::Reviews,
            ))
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
        Some(Ok(outcome)) => {
            // Replace the loading frame with the dashboard, then leave it inline.
            let ascii = ascii_mode(cli.ascii, program.screen().color_profile());
            let status = outcome.status();
            let warnings = outcome.warnings();
            render_dashboard(
                program.screen_mut(),
                &outcome.sections,
                &Ui::once(cli),
                &Changes::default(),
                &warnings,
                &status,
                None,
                ascii,
                false,
            )?;
            if outcome.has_errors() {
                Err(anyhow::anyhow!("{status}"))
            } else {
                Ok(Some(outcome))
            }
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
    // Always finish, but preserve a session error if teardown also failed.
    let finish = program.finish();
    let sections = result?;
    finish?;
    if let Some(outcome) = sections
        && outcome.issues.is_empty()
        && !cli.no_cache
    {
        cache::save(repo, cli.required, &outcome.sections);
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
    row_positions: &mut Vec<u16>,
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
        .chain(
            stats
                .releases
                .iter()
                .filter_map(|release| release.count.as_ref()),
        )
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
    let value = |c: Option<&commits::Count>| match c {
        Some(c) => count(c),
        None => "unavailable".to_string(),
    };
    let mut rows: Vec<(String, Option<String>, String, Option<String>)> = vec![(
        "upcoming".to_string(),
        stats.upcoming.as_ref().map(|b| b.url.clone()),
        value(stats.upcoming.as_ref().map(|bucket| &bucket.count)),
        None,
    )];
    for r in &stats.releases {
        rows.push((
            r.tag.clone(),
            Some(r.url.clone()),
            value(r.count.as_ref()),
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
        if url.is_some() {
            row_positions.push(y);
        }
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
    /// `Ctrl-R`: refresh now.
    Refresh,
    /// `r`: mark the selected PR as read.
    MarkRead,
    /// `R`: mark all PRs as read.
    MarkAllRead,
    /// `?`: toggle the help legend.
    ToggleHelp,
    /// `Tab`: switch to the other view.
    SwitchView,
    /// `Enter`: open the selected row in the browser.
    Open,
    /// `Shift+Enter`: open every link in the section the cursor is in.
    OpenSection,
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
    /// Page up or down without changing the selected link.
    Scroll(bool),
    /// `Ctrl-Z`: suspend to the shell, then resume.
    Suspend,
    /// The terminal was resized to these cell dimensions.
    Resize(u16, u16),
}

/// A keystroke while the search prompt is open (raw text input, unlike the
/// semantic [`Action`]s of normal mode).
#[derive(Debug, PartialEq, Eq)]
enum SearchAction<'a> {
    /// Ignore (an unbound key, or a non-input event).
    None,
    /// Produced text to append to the query.
    Text(&'a str),
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
    Scroll(bool),
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
            } else if k.matches("ctrl+r") {
                Action::Refresh
            } else if k.matches("R") {
                Action::MarkAllRead
            } else if k.matches("r") {
                Action::MarkRead
            } else if k.matches("?") {
                Action::ToggleHelp
            } else if k.matches("tab") {
                Action::SwitchView
            } else if k.matches("shift+enter") {
                Action::OpenSection
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
            } else if k.matches("pageup") {
                Action::Scroll(false)
            } else if k.matches("pagedown") {
                Action::Scroll(true)
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
fn classify_search(ev: &Event) -> SearchAction<'_> {
    match ev {
        Event::KeyPress(k) => {
            if let Some(text) = k.text.as_deref() {
                SearchAction::Text(text)
            } else if k.matches("backspace") {
                SearchAction::Backspace
            } else if k.matches_any(["enter", "shift+enter"]) {
                SearchAction::Enter
            } else if k.matches("esc") {
                SearchAction::Esc
            } else if k.matches("ctrl+c") {
                SearchAction::Quit
            } else if k.matches("ctrl+z") {
                SearchAction::Suspend
            } else if k.matches("pageup") {
                SearchAction::Scroll(false)
            } else if k.matches("pagedown") {
                SearchAction::Scroll(true)
            } else {
                SearchAction::None
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
    /// `Ctrl-R` was pressed: refresh now.
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
    /// Explicit body offset after page keys or the wheel; keyboard movement clears it.
    pub scroll: Option<u16>,
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
            scroll: None,
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

    fn selected_url(&self, good: &Sections) -> Option<String> {
        let selected = self.selected?;
        let mut filtered = None;
        let shown = self.shown(good, &mut filtered);
        nav::targets_visible(self.view, shown, &self.search, Visibility::all(shown))
            .get(selected)
            .map(|target| target.url.to_string())
    }

    fn restore_selection(&mut self, good: &Sections, url: Option<&str>) {
        self.selected = url.and_then(|url| {
            let mut filtered = None;
            let shown = self.shown(good, &mut filtered);
            nav::target_index(self.view, shown, &self.search, Visibility::all(shown), url)
        });
    }

    fn scroll_page(&mut self, down: bool, body_h: u16, available: u16, selected_row: Option<u16>) {
        self.scroll_by(down, available.max(1), body_h, available, selected_row);
    }

    fn scroll_by(
        &mut self,
        down: bool,
        step: u16,
        body_h: u16,
        available: u16,
        selected_row: Option<u16>,
    ) {
        let max = body_h.saturating_sub(available);
        let offset = self
            .scroll
            .unwrap_or_else(|| render::body_scroll_offset(body_h, available, selected_row))
            .min(max);
        self.scroll = Some(if down {
            offset.saturating_add(step).min(max)
        } else {
            offset.saturating_sub(step)
        });
    }

    /// Apply a watch-frame mouse event. Return the exact painted hyperlink,
    /// if any, for the caller to open.
    #[allow(clippy::too_many_arguments)]
    pub fn mouse_event(
        &mut self,
        screen: &impl TextSurface,
        sections: &Sections,
        status: &str,
        footer: Option<(&str, bool)>,
        ascii: bool,
        event: &Event,
    ) -> Option<String> {
        let (mouse, down) = match event {
            Event::MouseWheel(mouse) if mouse.button == MouseButton::WheelUp => {
                (mouse, Some(false))
            }
            Event::MouseWheel(mouse) if mouse.button == MouseButton::WheelDown => {
                (mouse, Some(true))
            }
            Event::MouseClick(mouse) if mouse.button == MouseButton::Left => (mouse, None),
            _ => return None,
        };
        let bounds = screen.bounds();
        let available = bounds
            .height
            .saturating_sub(bottom_height(self, status, footer, false) as u16);
        let mut filtered = None;
        let shown = self.shown(sections, &mut filtered);
        let (_, height, selected, _, rows) =
            watch_body(screen, shown, self, &Changes::default(), &[], ascii);
        if let Some(down) = down {
            self.scroll_by(down, 3, height, available, selected);
            return None;
        }
        if mouse.x >= bounds.width || mouse.y >= available {
            return None;
        }
        let offset = self
            .scroll
            .unwrap_or_else(|| render::body_scroll_offset(height, available, selected))
            .min(height.saturating_sub(available));
        let y = mouse.y + offset;
        if y >= height {
            return None;
        }
        let tab_row = if self.show_help {
            render::help_height(self.view) as u16 + 1
        } else {
            0
        };
        if y == tab_row {
            if let Some(view) = render::tab_at(screen, self.view, ascii, mouse.x) {
                self.view = view;
                self.selected = None;
                self.scroll = None;
                self.searching = false;
            }
            return None;
        }
        if let Some(index) = rows.iter().position(|row| *row == y) {
            self.selected = Some(index);
            self.scroll = Some(offset);
            self.searching = false;
        }
        screen
            .cell(Position::new(mouse.x, mouse.y))
            .and_then(|cell| cell.style.link.as_ref())
            .map(|link| link.url.clone())
    }

    fn targets<'a>(
        &self,
        shown: &'a Sections,
        visible: Visibility,
        section: bool,
    ) -> Vec<nav::Target<'a>> {
        if section {
            return nav::section_at_visible(
                self.view,
                shown,
                &self.search,
                self.selected.unwrap_or_default(),
                visible,
            );
        }
        let targets = nav::targets_visible(self.view, shown, &self.search, visible);
        self.selected
            .and_then(|index| targets.get(index).copied())
            .into_iter()
            .collect()
    }

    fn copy_text(
        &self,
        shown: &Sections,
        visible: Visibility,
        format: &str,
        section: bool,
    ) -> Option<(String, usize)> {
        let targets = self.targets(shown, visible, section);
        if targets.is_empty() {
            return None;
        }
        let text = targets
            .iter()
            .map(|target| {
                let link = target.format(format);
                if section { format!("- {link}") } else { link }
            })
            .collect::<Vec<_>>()
            .join("\n");
        Some((text, targets.len()))
    }

    fn open_links(
        &self,
        shown: &Sections,
        visible: Visibility,
        section: bool,
        mut open: impl FnMut(&str) -> std::io::Result<()>,
    ) -> Result<()> {
        for target in self.targets(shown, visible, section) {
            open(target.url).with_context(|| format!("opening {}", target.url))?;
        }
        Ok(())
    }

    fn mark_read(&self, shown: &Sections, visible: Visibility, unread: &mut Changes, all: bool) {
        if all {
            *unread = Changes::default();
        } else if let Some(selected) = self.selected
            && let Some(number) = nav::targets_visible(self.view, shown, &self.search, visible)
                .get(selected)
                .and_then(|target| target.number)
        {
            unread.mark_read(number);
        }
    }
}

/// Entry point: authenticate, resolve repo + user, then render once or watch.
pub fn run() -> Result<()> {
    let cli = Cli::load()?;
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
        // Only the selected view's sections are fetched (you can't Tab in
        // one-shot output).
        let outcome = fetch(
            &cli,
            &client,
            &repo,
            &me,
            &mut None,
            cli.view == View::Mine,
            cli.view == View::Reviews,
        );
        let status = outcome.status();
        let warnings = outcome.warnings();
        if !cli.no_cache && outcome.issues.is_empty() {
            cache::save(&repo, cli.required, &outcome.sections);
        }
        render_once(
            &terminal,
            &outcome.sections,
            &cli,
            &Changes::default(),
            &warnings,
            &status,
            None,
        )?;
        return if outcome.has_errors() {
            Err(anyhow::anyhow!("{status}"))
        } else {
            Ok(())
        };
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
    default_branch: Option<String>,
    /// The constant next-refresh ETA shown in the key-hint footer.
    eta: String,
    /// Change-detection baseline and the latest combined section data.
    prev: Option<Tracker>,
    last_good: Option<Sections>,
    /// Unread changes stay marked until acknowledged, for this session only.
    unread: Changes,
    /// The interactive dashboard state: view, help visibility, selection, search.
    ui: Ui,
    /// A transient action message, such as a copy confirmation or open error.
    last_status: String,
    /// Section errors remain visible until the next refresh resolves them.
    fetch_issues: Vec<FetchIssue>,
    /// Which views each refresh fetches, and when each was last fetched.
    schedule: Schedule,
    /// Whether a fetch is in flight, so the footer can say `^R refreshing`.
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
            default_branch: None,
            prev: None,
            last_good: None,
            unread: Changes::default(),
            ui: Ui {
                view: cli.view,
                show_help: false,
                selected: None,
                scroll: None,
                search: String::new(),
                searching: false,
                branch: cli.branch,
            },
            last_status: String::new(),
            fetch_issues: Vec::new(),
            schedule: Schedule::default(),
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
            .then(|| cache::load(self.repo, self.cli.required, self.cli.sort_open))
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
                self.redraw()?;
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
            self.program.enable_mouse(MouseTracking::empty())?;
            self.in_alt = true;
        }
        Ok(())
    }

    /// Paint the current dashboard via [`render_dashboard`], drawing the last
    /// good sections (or an empty frame, so a first-fetch error still shows its
    /// error line + footer) with unread changes highlighted.
    fn redraw(&mut self) -> Result<()> {
        let good = self.last_good.as_ref().unwrap_or(&Sections::EMPTY);
        let mut buf = None;
        let sections = self.ui.shown(good, &mut buf);
        let ascii = ascii_mode(self.cli.ascii, self.program.screen().color_profile());
        let status = self.status();
        let warnings: Vec<_> = self
            .fetch_issues
            .iter()
            .filter(|issue| issue.warning)
            .map(|issue| issue.scope)
            .collect();
        let caret = render_dashboard(
            self.program.screen_mut(),
            sections,
            &self.ui,
            &self.unread,
            &warnings,
            &status,
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
    /// user presses a quit key. A `Ctrl-R` during the wait refreshes both views.
    fn run(&mut self) -> Result<()> {
        let mut manual = false;
        loop {
            if termination_requested() {
                return Ok(());
            }
            if let Flow::Quit = self.fetch_responsive(manual)? {
                return Ok(());
            }
            match self.wait_interval()? {
                Flow::Quit => return Ok(()),
                flow => manual = matches!(flow, Flow::Refresh),
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
    /// `me` and the default branch (when shipments need it) are resolved here
    /// too (once), so even the first round-trip never freezes input. `Ctrl-R`
    /// is ignored — a fetch is already in flight.
    fn fetch_responsive(&mut self, manual: bool) -> Result<Flow> {
        // The footer says `^R refreshing` (with `^R` dimmed) for the duration.
        self.refreshing = true;
        self.repaint_last()?;
        let flow = self.fetch_loop(manual);
        self.refreshing = false;
        flow
    }

    /// The fetch + input-poll loop itself; [`Self::fetch_responsive`] wraps it to
    /// keep the `refreshing` footer state balanced on every exit path.
    fn fetch_loop(&mut self, manual: bool) -> Result<Flow> {
        let (cli, client, repo) = (self.cli.clone(), self.client.clone(), self.repo.clone());
        let mut me = self.me.clone();
        let mut default_branch = self.default_branch.clone();
        let resolve = me.is_empty();
        let due = self.schedule.next(self.ui.view, manual);

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let fetched = (|| {
                if resolve {
                    me = client.me()?;
                }
                let outcome = fetch(
                    &cli,
                    &client,
                    &repo,
                    &me,
                    &mut default_branch,
                    due.mine,
                    due.reviews,
                );
                Ok((me, default_branch, outcome))
            })();
            let _ = tx.send(fetched); // ignored if we already quit (rx dropped)
        });

        loop {
            if termination_requested() {
                return Ok(Flow::Quit);
            }
            match rx.try_recv() {
                Ok(Ok((me, default_branch, outcome))) => {
                    self.me = me;
                    self.default_branch = default_branch;
                    // Cleared before painting, so the result frame already shows
                    // the plain `^R refresh` hint again.
                    self.refreshing = false;
                    self.schedule
                        .record(due, &outcome.issues, chrono::Utc::now());
                    self.apply(outcome, due)?;
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

    /// Wait out the refresh interval, staying responsive: `Ctrl-R` returns
    /// [`Flow::Refresh`] to refresh now, `?` toggles help, quit/suspend/resize
    /// are honored, other keys are discarded.
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
                    Flow::Continue => {}
                    flow => return Ok(flow),
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
        if matches!(ev, Event::KittyKeyboardEnhancements(_)) {
            self.program
                .set_kitty_keyboard(Some(KittyKeyboardFlags::DISAMBIGUATE_ESCAPE_CODES))?;
            return Ok(Flow::Continue);
        }
        if self.in_alt {
            match ev {
                Event::MouseClick(_) | Event::MouseWheel(_) => {
                    let good = self.last_good.as_ref().unwrap_or(&Sections::EMPTY);
                    let screen = self.program.screen();
                    let ascii = ascii_mode(self.cli.ascii, screen.color_profile());
                    let status = self.status();
                    let url = self.ui.mouse_event(
                        screen,
                        good,
                        &status,
                        Some((self.eta.as_str(), self.refreshing)),
                        ascii,
                        ev,
                    );
                    if let Some(url) = url
                        && let Err(error) = open::url(&url)
                    {
                        self.last_status = format!("error: open failed: opening {url}: {error}");
                    }
                    self.repaint_last()?;
                    return Ok(Flow::Continue);
                }
                _ => {}
            }
        }
        if self.ui.searching {
            return self.handle_search_event(ev);
        }
        match classify(ev) {
            Action::Quit => return Ok(Flow::Quit),
            Action::Refresh => return Ok(Flow::Refresh),
            Action::Suspend => self.suspend()?,
            Action::ToggleHelp => {
                self.ui.show_help = !self.ui.show_help;
                self.ui.selected = None;
                self.ui.scroll = None;
                self.repaint_last()?;
            }
            Action::SwitchView => {
                // Selection indices don't carry across views, so start fresh.
                self.ui.view = self.ui.view.toggle();
                self.ui.selected = None;
                self.ui.scroll = None;
                self.repaint_last()?;
            }
            Action::Search => {
                self.ui.searching = true;
                self.ui.selected = None;
                self.ui.scroll = None;
                self.repaint_last()?;
            }
            Action::Cancel => {
                // Esc clears an applied filter; with none to clear, it quits.
                if self.ui.search.is_empty() {
                    return Ok(Flow::Quit);
                }
                self.ui.search.clear();
                self.ui.selected = None;
                self.ui.scroll = None;
                self.repaint_last()?;
            }
            Action::Open => self.open_links(false)?,
            Action::OpenSection => self.open_links(true)?,
            Action::Copy => self.copy_links(false)?,
            Action::CopySection => self.copy_links(true)?,
            Action::MarkRead => self.mark_read(false)?,
            Action::MarkAllRead => self.mark_read(true)?,
            Action::Move(m) => {
                let len = self.target_count();
                let next = nav::moved(m, self.ui.selected, len, self.half_page());
                if next != self.ui.selected || self.ui.scroll.is_some() {
                    self.ui.selected = next;
                    self.ui.scroll = None;
                    self.repaint_last()?;
                }
            }
            Action::Scroll(down) => self.scroll_page(down)?,
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
            }
            Action::None => {}
        }
        Ok(Flow::Continue)
    }

    /// Apply a keystroke while the search prompt is open. Typing filters live
    /// (resetting the cursor), Enter applies the filter and closes the prompt,
    /// Esc clears the filter and closes it.
    fn handle_search_event(&mut self, ev: &Event) -> Result<Flow> {
        match classify_search(ev) {
            SearchAction::Text(text) => {
                self.ui.search.push_str(text);
                self.ui.selected = None;
                self.ui.scroll = None;
            }
            SearchAction::Backspace => {
                self.ui.search.pop();
                self.ui.selected = None;
                self.ui.scroll = None;
            }
            SearchAction::Enter => {
                self.ui.searching = false;
            }
            SearchAction::Esc => {
                self.ui.search.clear();
                self.ui.searching = false;
                self.ui.scroll = None;
            }
            SearchAction::Quit => return Ok(Flow::Quit),
            SearchAction::Suspend => return self.suspend().map(|()| Flow::Continue),
            SearchAction::Scroll(down) => return self.scroll_page(down).map(|()| Flow::Continue),
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
        let visible = Visibility::all(shown);
        nav::targets_visible(self.ui.view, shown, &self.ui.search, visible).len()
    }

    fn selected_url(&self) -> Option<String> {
        self.ui.selected_url(self.last_good.as_ref()?)
    }

    fn restore_selection(&mut self, url: Option<&str>) {
        self.ui
            .restore_selection(self.last_good.as_ref().unwrap_or(&Sections::EMPTY), url);
    }

    /// The half-page movement step: half the terminal window's rows.
    fn half_page(&self) -> usize {
        self.program
            .window_cells()
            .map_or(10, |s| usize::from(s.height / 2).max(1))
    }

    fn scroll_page(&mut self, down: bool) -> Result<()> {
        if !self.in_alt {
            return Ok(());
        }
        let good = self.last_good.as_ref().unwrap_or(&Sections::EMPTY);
        let mut filtered = None;
        let sections = self.ui.shown(good, &mut filtered);
        let screen = self.program.screen();
        let ascii = ascii_mode(self.cli.ascii, screen.color_profile());
        let (_, body_h, selected, _, _) =
            watch_body(screen, sections, &self.ui, &self.unread, &[], ascii);
        let status = self.status();
        let bottom_h = bottom_height(
            &self.ui,
            &status,
            Some((self.eta.as_str(), self.refreshing)),
            false,
        ) as u16;
        let available = screen.height().saturating_sub(bottom_h);
        self.ui.scroll_page(down, body_h, available, selected);
        self.repaint_last()
    }

    /// Copy the selected link (`y`) or its whole section as a list (`Y`).
    fn copy_links(&mut self, section: bool) -> Result<()> {
        let Some(good) = &self.last_good else {
            return Ok(());
        };
        let mut filtered = None;
        let shown = self.ui.shown(good, &mut filtered);
        let visible = Visibility::all(shown);
        let Some((text, n)) = self
            .ui
            .copy_text(shown, visible, &self.cli.link_format, section)
        else {
            return Ok(());
        };
        let plural = if n == 1 { "" } else { "s" };
        self.last_status = match clipboard::copy(&text) {
            Ok(()) => format!("copied {n} link{plural}"),
            Err(e) => format!("error: copy failed: {e}"),
        };
        self.repaint_last()
    }

    /// Open the selected row or section. Stop at the first failure and show it
    /// in the dim error line; no targets leaves the screen as is.
    fn open_links(&mut self, section: bool) -> Result<()> {
        let Some(good) = &self.last_good else {
            return Ok(());
        };
        let mut filtered = None;
        let shown = self.ui.shown(good, &mut filtered);
        let visible = Visibility::all(shown);
        if let Err(e) = self.ui.open_links(shown, visible, section, open::url) {
            self.last_status = format!("error: open failed: {e:#}");
            self.ui.selected = None;
            self.repaint_last()?;
        }
        Ok(())
    }

    fn mark_read(&mut self, all: bool) -> Result<()> {
        let good = self.last_good.as_ref().unwrap_or(&Sections::EMPTY);
        let mut filtered = None;
        let shown = self.ui.shown(good, &mut filtered);
        let visible = Visibility::all(shown);
        self.ui.mark_read(shown, visible, &mut self.unread, all);
        self.repaint_last()
    }

    /// Render a successful fetch: diff against the previous snapshot, paint, ring
    /// the bell on a change (once armed), and cache the result. Views the
    /// refresh skipped keep their previous sections and errors.
    fn apply(&mut self, outcome: FetchOutcome, due: Due) -> Result<()> {
        let selected = self.selected_url();
        let outcome = outcome.merge(
            self.last_good.take(),
            std::mem::take(&mut self.fetch_issues),
            due,
        );
        let FetchOutcome { sections, issues } = outcome;
        let mut tracker = Tracker::build(sections.prs.as_deref(), sections.merged.as_deref());
        tracker.preserve_untrusted(
            self.prev.as_ref(),
            issues.iter().any(|issue| issue.scope == FetchScope::Mine),
            issues.iter().any(|issue| issue.scope == FetchScope::Merged),
        );
        let changes = Tracker::advance(&mut self.prev, tracker);
        let bell = changes.any();
        self.unread.extend(changes);

        self.last_status.clear();
        self.fetch_issues = issues;
        self.last_good = Some(sections);
        // Responsive queue membership can change as checks start or finish.
        // Keep the same URL selected instead of reusing its old numeric index.
        self.restore_selection(selected.as_deref());
        self.enter_alt()?;
        self.redraw()?;

        if self.armed && bell && self.cli.bell && !self.cli.no_bell {
            let _ = self.program.beep();
        }
        self.armed = true;
        if self.fetch_issues.is_empty()
            && !self.cli.no_cache
            && let Some(good) = &self.last_good
        {
            cache::save(self.repo, self.cli.required, good);
        }
        Ok(())
    }

    fn status(&self) -> String {
        self.fetch_issues
            .iter()
            .map(FetchIssue::line)
            .chain(self.schedule.age_note(self.ui.view, chrono::Utc::now()))
            .chain((!self.last_status.is_empty()).then(|| self.last_status.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Render a failed fetch: keep the last good data, add a dim error line, and
    /// do not ring. With no data yet, just the error line and footer show.
    fn show_error(&mut self, e: anyhow::Error) -> Result<()> {
        self.last_status = format!("error: {}", short_error(&e));
        self.enter_alt()?;
        self.ui.selected = None;
        self.redraw()
    }

    /// Repaint the dashboard, including an initial error without cached data.
    /// Leave the inline loading frame alone until the alternate screen is active.
    fn repaint_last(&mut self) -> Result<()> {
        if self.in_alt {
            self.redraw()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use uncurses::buffer::Surface;
    use uncurses::text::Encode;

    /// Paint a dashboard onto an offscreen buffer and read it back as plain text.
    fn body(sections: &Sections, ui: &Ui) -> String {
        let mut canvas = TextBuffer::new(render::OUTPUT_WIDTH as u16, 64);
        let (used, _) = paint_dashboard(
            &mut canvas,
            sections,
            ui,
            &Changes::default(),
            RenderMessages {
                warnings: &[],
                status: "",
            },
            None,
            true,
        );
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

        let ctrl_c = Event::KeyPress(Key::new(KeyCode::Char('c'), KeyModifiers::CTRL).normalized());
        let q = Event::KeyPress(Key::new(KeyCode::Char('q'), KeyModifiers::empty()).normalized());

        assert_eq!(classify_search(&ctrl_c), SearchAction::Quit);
        assert_eq!(classify_search(&q), SearchAction::Text("q"));
    }

    #[test]
    fn search_uses_produced_text_instead_of_key_codes() {
        use uncurses::event::Key;

        for (code, text) in [
            (KeyCode::Space, " "),
            (KeyCode::Char('R'), "R"),
            (KeyCode::Char('!'), "!"),
        ] {
            let event = Event::KeyPress(Key::new(code, KeyModifiers::empty()).normalized());
            assert_eq!(classify_search(&event), SearchAction::Text(text));
        }

        for text in ["!", "e\u{301}", "enter", "backspace", "esc"] {
            let mut key = Key::new(KeyCode::Char('1'), KeyModifiers::SHIFT);
            key.text = Some(text.to_owned());
            let event = Event::KeyPress(key.normalized());
            assert_eq!(classify_search(&event), SearchAction::Text(text));
        }
    }

    #[test]
    fn search_matches_commands_and_ignores_non_text_keys() {
        use uncurses::event::Key;

        for (code, modifiers, expected) in [
            (
                KeyCode::Backspace,
                KeyModifiers::empty(),
                SearchAction::Backspace,
            ),
            (KeyCode::Escape, KeyModifiers::empty(), SearchAction::Esc),
            (
                KeyCode::Char('z'),
                KeyModifiers::CTRL,
                SearchAction::Suspend,
            ),
            (KeyCode::Char('x'), KeyModifiers::ALT, SearchAction::None),
            (KeyCode::Space, KeyModifiers::CTRL, SearchAction::None),
            (KeyCode::Up, KeyModifiers::empty(), SearchAction::None),
        ] {
            let event = Event::KeyPress(Key::new(code, modifiers).normalized());
            assert_eq!(classify_search(&event), expected, "{event:?}");
        }
    }

    #[test]
    fn shift_enter_opens_a_section_but_only_applies_an_active_search() {
        use uncurses::event::Key;

        let enter = Event::KeyPress(Key::new(KeyCode::Enter, KeyModifiers::empty()).normalized());
        let shift_enter =
            Event::KeyPress(Key::new(KeyCode::Enter, KeyModifiers::SHIFT).normalized());
        assert!(matches!(classify(&enter), Action::Open), "{enter:?}");
        assert!(
            matches!(classify(&shift_enter), Action::OpenSection),
            "{shift_enter:?}"
        );
        assert_eq!(classify_search(&enter), SearchAction::Enter);
        assert_eq!(classify_search(&shift_enter), SearchAction::Enter);
        for modifiers in [
            KeyModifiers::CTRL,
            KeyModifiers::ALT,
            KeyModifiers::CTRL | KeyModifiers::SHIFT,
        ] {
            let event = Event::KeyPress(Key::new(KeyCode::Enter, modifiers).normalized());
            assert!(matches!(classify(&event), Action::None), "{event:?}");
            assert_eq!(classify_search(&event), SearchAction::None);
        }
    }

    #[test]
    fn read_keys_are_distinct_from_refresh_and_search_text() {
        use uncurses::event::Key;

        let read =
            Event::KeyPress(Key::new(KeyCode::Char('r'), KeyModifiers::empty()).normalized());
        let refresh =
            Event::KeyPress(Key::new(KeyCode::Char('r'), KeyModifiers::CTRL).normalized());
        assert!(matches!(classify(&read), Action::MarkRead));
        assert!(matches!(classify(&refresh), Action::Refresh));
        assert_eq!(classify_search(&read), SearchAction::Text("r"));
        assert_eq!(classify_search(&refresh), SearchAction::None);
        for modifiers in [
            KeyModifiers::empty(),
            KeyModifiers::SHIFT,
            KeyModifiers::CAPS_LOCK,
        ] {
            let read_all = Event::KeyPress(Key::new(KeyCode::Char('R'), modifiers).normalized());
            assert!(
                matches!(classify(&read_all), Action::MarkAllRead),
                "{read_all:?}"
            );
            assert_eq!(classify_search(&read_all), SearchAction::Text("R"));
        }
    }

    /// A `Ui` for the given view with nothing selected and no filter.
    fn ui(view: View) -> Ui {
        Ui {
            view,
            show_help: false,
            selected: None,
            scroll: None,
            search: String::new(),
            searching: false,
            branch: false,
        }
    }

    #[test]
    fn bottom_height_matches_painted_blocks() {
        for view in [View::Mine, View::Reviews] {
            for bits in 0u8..32 {
                let mut ui = ui(view);
                ui.show_help = bits & 1 != 0;
                if bits & 2 != 0 {
                    ui.search = "query".into();
                }
                ui.searching = bits & 4 != 0;
                let status = match (bits & 8 != 0, bits & 16 != 0) {
                    (true, true) => "error: offline\nerror: stale",
                    (true, false) => "error: offline",
                    (false, _) => "",
                };
                let footer = (bits & 16 != 0).then_some(("5m", false));
                let mut canvas = TextBuffer::new(120, 16);
                let (used, _) = paint_bottom(
                    &mut canvas,
                    &Sections::EMPTY,
                    &ui,
                    status,
                    footer,
                    true,
                    ui.show_help,
                    Visibility::default(),
                    false,
                    0,
                );
                assert_eq!(
                    bottom_height(&ui, status, footer, ui.show_help),
                    usize::from(used),
                    "{view:?}, help/search/searching/status/footer bits={bits:05b}"
                );
            }
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
            missing_required_checks: 0,
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
            created_at: None,
        }
    }

    fn merged_row(n: i64) -> merged::MergedRow {
        merged::MergedRow {
            number: n,
            title: format!("merged-{n}"),
            branch: String::new(),
            url: format!("https://merged/{n}"),
            release: None,
            release_unknown: false,
            merged_at: None,
        }
    }

    fn review_row(n: i64) -> reviews::ReviewRow {
        reviews::ReviewRow {
            number: n,
            title: format!("review-{n:02}"),
            branch: String::new(),
            author: "other".into(),
            url: format!("https://review/{n}"),
            state: status::ReviewState::Awaiting,
            is_draft: false,
            updated_at: None,
        }
    }

    #[test]
    fn mark_read_clears_only_the_selected_visible_pr() {
        let sections = Sections {
            prs: Some(vec![queued_open_row(1)]),
            queue: Some(vec![queue_row(1, true, true), queue_row(2, false, false)]),
            merged: Some(vec![merged_row(3), merged_row(4)]),
            ..Sections::EMPTY
        };
        let mut ui = ui(View::Mine);
        let visible = Visibility::all(&sections);
        let mut unread = Changes {
            status_changed: [1, 2].into(),
            newly_merged: [3, 4].into(),
        };
        // The queued open PR is deduplicated: index 2 is the first merged PR.
        ui.selected = Some(2);
        let frame = watch_frame(&sections, &ui, 120, 2);
        assert!(
            frame
                .display_with(Profile::Disabled)
                .to_string()
                .contains("merged-3")
        );
        assert_eq!(
            ui.copy_text(&sections, visible, "{url}", false),
            Some(("https://merged/3".into(), 1))
        );
        assert_eq!(
            ui.copy_text(&sections, visible, "{url}", true),
            Some(("- https://merged/3\n- https://merged/4".into(), 2))
        );
        ui.mark_read(&sections, visible, &mut unread, false);
        assert_eq!(unread.status_changed, [1, 2].into());
        assert_eq!(unread.newly_merged, [4].into());
        assert_eq!(ui.selected, Some(2));

        ui.selected = Some(0);
        ui.mark_read(&sections, visible, &mut unread, false);
        assert_eq!(unread.status_changed, [2].into());
        assert_eq!(unread.newly_merged, [4].into());

        ui.search = "merged-4".into();
        let mut filtered = None;
        let shown = ui.shown(&sections, &mut filtered);
        ui.mark_read(shown, Visibility::all(shown), &mut unread, false);
        assert_eq!(unread.status_changed, [2].into());
        assert!(unread.newly_merged.is_empty());
    }

    #[test]
    fn mark_read_ignores_absent_selection_and_shipments() {
        let sections = Sections {
            merged: Some(vec![merged_row(1)]),
            commits: Some(commits::CommitStats {
                available: true,
                upcoming: Some(commits::Bucket {
                    count: commits::Count {
                        mine: 1,
                        capped: false,
                    },
                    url: "https://compare/1".into(),
                }),
                releases: vec![],
            }),
            ..Sections::EMPTY
        };
        let visible = Visibility::all(&sections);
        let mut ui = ui(View::Mine);
        let mut unread = Changes {
            newly_merged: [1].into(),
            ..Changes::default()
        };
        for selected in [None, Some(1), Some(2)] {
            ui.selected = selected;
            ui.mark_read(&sections, visible, &mut unread, false);
            assert_eq!(unread.newly_merged, [1].into(), "selected={selected:?}");
        }
        ui.selected = Some(0);
        ui.search = "no matches".into();
        ui.mark_read(&sections, visible, &mut unread, false);
        assert_eq!(unread.newly_merged, [1].into());
    }

    #[test]
    fn mark_all_read_includes_hidden_and_filtered_prs_in_either_view() {
        let sections = Sections {
            prs: Some(vec![queued_open_row(1)]),
            merged: Some(vec![merged_row(2)]),
            ..Sections::EMPTY
        };
        for view in [View::Mine, View::Reviews] {
            let ui = Ui {
                search: "no matches".into(),
                ..ui(view)
            };
            let mut unread = Changes {
                status_changed: [1].into(),
                newly_merged: [2].into(),
            };
            let mut filtered = None;
            let shown = ui.shown(&sections, &mut filtered);
            ui.mark_read(shown, Visibility::default(), &mut unread, true);
            assert!(!unread.any(), "{view:?}: {unread:?}");
        }
    }

    #[test]
    fn unread_marker_survives_selection_help_and_search_until_marked_read() {
        let sections = Sections {
            merged: Some(vec![merged_row(1), merged_row(2)]),
            ..Sections::EMPTY
        };
        let mut unread = Changes {
            newly_merged: [2].into(),
            ..Changes::default()
        };
        let mut ui = ui(View::Mine);
        for selected in [None, Some(1)] {
            ui.selected = selected;
            ui.show_help = selected.is_some();
            let text = render_to_string(&sections, &ui, &unread, None, true, Profile::Disabled);
            let row = text.lines().find(|row| row.contains("merged-2")).unwrap();
            assert!(row.starts_with('>'), "{row}");
        }
        ui.search = "merged-2".into();
        ui.selected = Some(0);
        let mut filtered = None;
        let shown = ui.shown(&sections, &mut filtered);
        let text = render_to_string(shown, &ui, &unread, None, true, Profile::Disabled);
        let row = text.lines().find(|row| row.contains("merged-2")).unwrap();
        assert!(row.starts_with('>'), "{row}");
        ui.mark_read(shown, Visibility::all(shown), &mut unread, false);
        let text = render_to_string(shown, &ui, &unread, None, true, Profile::Disabled);
        let row = text.lines().find(|row| row.contains("merged-2")).unwrap();
        assert!(!row.starts_with('>'), "{row}");
        assert_eq!(ui.selected, Some(0));
    }

    #[test]
    fn copy_formats_selected_links_and_section_lists() {
        let sections = Sections {
            merged: Some(vec![merged_row(1), merged_row(2)]),
            ..Sections::EMPTY
        };
        let ui = Ui {
            selected: Some(1),
            ..ui(View::Mine)
        };
        let visible = Visibility::all(&sections);
        let default = Cli::try_parse_from(["prowl"]).unwrap();
        let markdown = Cli::try_parse_from(["prowl", "--link-format", "[{title}]({url})"]).unwrap();
        for (format, section, expected, count) in [
            (default.link_format.as_str(), false, "https://merged/2", 1),
            (
                default.link_format.as_str(),
                true,
                "- https://merged/1\n- https://merged/2",
                2,
            ),
            (
                markdown.link_format.as_str(),
                false,
                "[merged-2](https://merged/2)",
                1,
            ),
            (
                markdown.link_format.as_str(),
                true,
                "- [merged-1](https://merged/1)\n- [merged-2](https://merged/2)",
                2,
            ),
        ] {
            assert_eq!(
                ui.copy_text(&sections, visible, format, section),
                Some((expected.into(), count)),
                "format={format:?}, section={section}"
            );
        }
        assert_eq!(ui.selected, Some(1));
    }

    #[test]
    fn copy_handles_absent_selection_and_empty_sections() {
        let sections = Sections {
            prs: Some(vec![]),
            merged: Some(vec![merged_row(1)]),
            ..Sections::EMPTY
        };
        let mut ui = ui(View::Mine);
        let visible = Visibility::all(&sections);
        assert_eq!(ui.copy_text(&sections, visible, "{url}", false), None);
        assert_eq!(
            ui.copy_text(&sections, visible, "{url}", true),
            Some(("- https://merged/1".into(), 1))
        );
        ui.selected = Some(1);
        for section in [false, true] {
            assert_eq!(ui.copy_text(&sections, visible, "{url}", section), None);
        }
        ui.selected = Some(0);
        ui.search = "no matches".into();
        for section in [false, true] {
            assert_eq!(ui.copy_text(&sections, visible, "{url}", section), None);
        }
    }

    #[test]
    fn open_and_copy_include_every_offscreen_search_match() {
        let sections = Sections {
            queue: Some((0..=3).map(|n| queue_row(n, n == 0, false)).collect()),
            ..Sections::EMPTY
        };
        let ui = Ui {
            search: "other".into(),
            selected: Some(1),
            ..ui(View::Mine)
        };
        let mut filtered = None;
        let shown = ui.shown(&sections, &mut filtered);
        let visible = Visibility::all(shown);
        assert_eq!(
            ui.copy_text(shown, visible, "[{title}]({url})", true),
            Some((
                "- [queue-1](https://queue/1)\n- [queue-2](https://queue/2)\n- [queue-3](https://queue/3)".into(),
                3,
            ))
        );
        assert_eq!(
            ui.copy_text(shown, visible, "[{title}]({url})", false),
            Some(("[queue-2](https://queue/2)".into(), 1))
        );
        for (section, expected) in [
            (false, vec!["https://queue/2"]),
            (
                true,
                vec!["https://queue/1", "https://queue/2", "https://queue/3"],
            ),
        ] {
            let mut opened = Vec::new();
            ui.open_links(shown, visible, section, |url| {
                opened.push(url.to_string());
                Ok(())
            })
            .unwrap();
            assert_eq!(opened, expected, "section={section}");
        }
    }

    #[test]
    fn open_links_use_the_selected_row_or_section_in_render_order() {
        let sections = Sections {
            prs: Some(vec![]),
            queue: Some(vec![queue_row(1, true, false), queue_row(2, false, false)]),
            merged: Some(vec![merged_row(3), merged_row(4)]),
            ..Sections::EMPTY
        };
        let visible = Visibility::all(&sections);
        for (selected, section, expected) in [
            (None, false, vec![]),
            (None, true, vec!["https://queue/1", "https://queue/2"]),
            (Some(1), false, vec!["https://queue/2"]),
            (Some(1), true, vec!["https://queue/1", "https://queue/2"]),
            (Some(2), true, vec!["https://merged/3", "https://merged/4"]),
            (Some(4), true, vec![]),
        ] {
            let ui = Ui {
                selected,
                ..ui(View::Mine)
            };
            let mut opened = Vec::new();
            ui.open_links(&sections, visible, section, |url| {
                opened.push(url.to_string());
                Ok(())
            })
            .unwrap();
            assert_eq!(opened, expected, "selected={selected:?}, section={section}");
            assert_eq!(ui.selected, selected);
        }
    }

    #[test]
    fn open_section_supports_reviews_and_shipments() {
        let sections = Sections {
            reviewed_merged: Some(
                (1..=2)
                    .map(|n| reviews::ReviewedMergedRow {
                        number: n,
                        title: format!("reviewed-{n}"),
                        branch: String::new(),
                        author: "other".into(),
                        url: format!("https://reviewed/{n}"),
                        merged_at: None,
                    })
                    .collect(),
            ),
            commits: Some(commits::CommitStats {
                available: true,
                upcoming: Some(commits::Bucket {
                    count: commits::Count {
                        mine: 1,
                        capped: false,
                    },
                    url: "https://compare/upcoming".into(),
                }),
                releases: vec![commits::Release {
                    tag: "v1".into(),
                    count: Some(commits::Count {
                        mine: 1,
                        capped: false,
                    }),
                    url: "https://releases/v1".into(),
                    published_at: None,
                }],
            }),
            ..Sections::EMPTY
        };
        for (view, expected) in [
            (
                View::Mine,
                vec!["https://compare/upcoming", "https://releases/v1"],
            ),
            (
                View::Reviews,
                vec!["https://reviewed/1", "https://reviewed/2"],
            ),
        ] {
            let mut opened = Vec::new();
            ui(view)
                .open_links(&sections, Visibility::all(&sections), true, |url| {
                    opened.push(url.to_string());
                    Ok(())
                })
                .unwrap();
            assert_eq!(opened, expected, "{view:?}");
        }
    }

    #[test]
    fn open_section_stops_at_the_first_failure_and_identifies_the_link() {
        let sections = Sections {
            merged: Some((1..=3).map(merged_row).collect()),
            ..Sections::EMPTY
        };
        let mut attempted = Vec::new();
        let error = ui(View::Mine)
            .open_links(&sections, Visibility::all(&sections), true, |url| {
                attempted.push(url.to_string());
                if attempted.len() == 2 {
                    Err(std::io::Error::other("opener unavailable"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
        assert_eq!(attempted, ["https://merged/1", "https://merged/2"]);
        assert_eq!(
            format!("{error:#}"),
            "opening https://merged/2: opener unavailable"
        );
    }

    #[test]
    fn open_section_with_no_matches_does_not_open_any_links() {
        let sections = Sections {
            merged: Some(vec![merged_row(1)]),
            ..Sections::EMPTY
        };
        let ui = Ui {
            search: "no matches".into(),
            ..ui(View::Mine)
        };
        ui.open_links(&sections, Visibility::all(&sections), true, |url| {
            panic!("unexpected open: {url}")
        })
        .unwrap();
    }

    fn watch_frame(sections: &Sections, ui: &Ui, width: u16, rows: u16) -> TextBuffer {
        let mut canvas = TextBuffer::new(width, rows);
        paint_watch_frame(
            &mut canvas,
            sections,
            ui,
            &Changes::default(),
            "",
            Some(("5m", false)),
            true,
        );
        canvas
    }

    fn mouse_click(
        ui: &mut Ui,
        sections: &Sections,
        screen: &TextBuffer,
        x: u16,
        y: u16,
    ) -> Option<String> {
        ui.mouse_event(
            screen,
            sections,
            "",
            Some(("5m", false)),
            true,
            &Event::MouseClick(uncurses::event::Mouse::new(
                x,
                y,
                MouseButton::Left,
                KeyModifiers::empty(),
            )),
        )
    }

    fn link_position(screen: &TextBuffer, url: &str) -> Position {
        (0..screen.height())
            .flat_map(|y| (0..screen.width()).map(move |x| Position::new(x, y)))
            .find(|position| {
                screen
                    .cell(*position)
                    .unwrap()
                    .style
                    .link
                    .as_ref()
                    .is_some_and(|link| link.url == url)
            })
            .unwrap_or_else(|| panic!("missing link: {url}"))
    }

    #[test]
    fn mouse_selects_rows_in_both_views_including_narrow_frames() {
        let sections = Sections {
            prs: Some(vec![queued_open_row(1)]),
            queue: Some(vec![queue_row(1, true, false), queue_row(2, false, false)]),
            merged: Some(vec![merged_row(3)]),
            reviews: Some(vec![review_row(4)]),
            reviewed_merged: Some(vec![reviews::ReviewedMergedRow {
                number: 5,
                title: "reviewed".into(),
                branch: String::new(),
                author: "other".into(),
                url: "https://reviewed/5".into(),
                merged_at: None,
            }]),
            ..Sections::EMPTY
        };
        for view in [View::Mine, View::Reviews] {
            let mut ui = ui(view);
            ui.show_help = true;
            let screen = watch_frame(&sections, &ui, 120, 40);
            let targets = nav::targets_visible(view, &sections, "", Visibility::all(&sections));
            for (index, target) in targets.iter().enumerate() {
                let position = link_position(&screen, target.url);
                for width in [120, 1] {
                    let narrow = TextBuffer::new(width, 40);
                    assert_eq!(
                        mouse_click(&mut ui, &sections, &narrow, width - 1, position.y),
                        None
                    );
                    assert_eq!(ui.selected, Some(index));
                    assert_eq!(ui.selected_url(&sections).as_deref(), Some(target.url));
                }
                assert_eq!(
                    mouse_click(&mut ui, &sections, &screen, position.x, position.y).as_deref(),
                    Some(target.url)
                );
            }
        }
    }

    #[test]
    fn mouse_opens_release_cells_instead_of_the_pr_and_skips_url_less_shipments() {
        let mut row = merged_row(1);
        row.release = Some(commits::ReleaseRef {
            tag: "v1".into(),
            url: "https://release/v1".into(),
        });
        let sections = Sections {
            merged: Some(vec![row]),
            commits: Some(commits::CommitStats {
                available: true,
                upcoming: None,
                releases: vec![commits::Release {
                    tag: "v1".into(),
                    count: Some(commits::Count {
                        mine: 1,
                        capped: false,
                    }),
                    url: "https://release/v1".into(),
                    published_at: None,
                }],
            }),
            ..Sections::EMPTY
        };
        let mut ui = ui(View::Mine);
        let screen = watch_frame(&sections, &ui, 120, 20);
        let release = link_position(&screen, "https://release/v1");
        assert_eq!(
            mouse_click(&mut ui, &sections, &screen, release.x, release.y).as_deref(),
            Some("https://release/v1")
        );
        assert_eq!(
            ui.selected_url(&sections).as_deref(),
            Some("https://merged/1")
        );
        let text = screen.display_with(Profile::Disabled).to_string();
        let upcoming = text
            .lines()
            .position(|line| line.contains("upcoming"))
            .unwrap() as u16;
        assert_eq!(mouse_click(&mut ui, &sections, &screen, 20, upcoming), None);
        assert_eq!(ui.selected, Some(0));
        assert_eq!(
            mouse_click(&mut ui, &sections, &screen, 119, upcoming + 1),
            None
        );
        assert_eq!(ui.selected, Some(1));
        assert_eq!(
            ui.selected_url(&sections).as_deref(),
            Some("https://release/v1")
        );
    }

    #[test]
    fn mouse_opens_the_displayed_link_after_a_width_policy_change() {
        use uncurses::text::WidthMode;

        let sections = Sections {
            commits: Some(commits::CommitStats {
                available: true,
                upcoming: Some(commits::Bucket {
                    count: commits::Count {
                        mine: 1,
                        capped: false,
                    },
                    url: "https://upcoming".into(),
                }),
                releases: vec![commits::Release {
                    tag: "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}v1".into(),
                    count: Some(commits::Count {
                        mine: 1,
                        capped: false,
                    }),
                    url: "https://release/v1".into(),
                    published_at: None,
                }],
            }),
            ..Sections::EMPTY
        };
        let mut ui = ui(View::Mine);
        let mut screen = watch_frame(&sections, &ui, 120, 20);
        assert_eq!(screen.width_mode(), WidthMode::Wc);
        let position = link_position(&screen, "https://upcoming");
        let last = Position::new(position.x + 7, position.y);
        assert_eq!(screen.cell(last).unwrap().content(), "g");

        screen.set_width_mode(WidthMode::Grapheme);

        assert_eq!(
            mouse_click(&mut ui, &sections, &screen, last.x, last.y).as_deref(),
            Some("https://upcoming")
        );
        assert_eq!(
            mouse_click(&mut ui, &sections, &screen, position.x - 1, position.y),
            None
        );
    }

    #[test]
    fn mouse_scroll_and_click_follow_the_filtered_viewport_not_the_pinned_footer() {
        let sections = Sections {
            merged: Some((1..=30).map(merged_row).collect()),
            ..Sections::EMPTY
        };
        let mut ui = Ui {
            search: "merged-2".into(),
            searching: true,
            selected: Some(0),
            ..ui(View::Mine)
        };
        let screen = TextBuffer::new(120, 8);
        let wheel = |button| {
            Event::MouseWheel(uncurses::event::Mouse::new(
                0,
                7,
                button,
                KeyModifiers::empty(),
            ))
        };
        ui.mouse_event(
            &screen,
            &sections,
            "",
            Some(("5m", false)),
            true,
            &wheel(MouseButton::WheelDown),
        );
        assert!(ui.scroll.is_some_and(|offset| offset > 0));
        assert_eq!(ui.selected, Some(0));
        assert!(ui.searching);
        let mut filtered = None;
        let shown = ui.shown(&sections, &mut filtered);
        let frame = watch_frame(shown, &ui, 120, 8);
        let text = frame.display_with(Profile::Disabled).to_string();
        let (y, line) = text
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("merged-2"))
            .unwrap();
        let number = line
            .split_whitespace()
            .next()
            .unwrap()
            .trim_start_matches('#');
        let url = format!("https://merged/{number}");
        let position = link_position(&frame, &url);
        assert_eq!(usize::from(position.y), y);
        let offset = ui.scroll;
        assert_eq!(
            mouse_click(&mut ui, &sections, &frame, 119, position.y),
            None
        );
        assert_eq!(ui.selected_url(&sections), Some(url));
        assert_eq!(ui.scroll, offset);
        assert!(!ui.searching);
        let selected = ui.selected;
        for (x, y) in [(0, 7), (120, 0), (0, 8)] {
            assert_eq!(mouse_click(&mut ui, &sections, &frame, x, y), None);
            assert_eq!(ui.selected, selected);
        }
        for _ in 0..20 {
            ui.mouse_event(
                &screen,
                &sections,
                "",
                Some(("5m", false)),
                true,
                &wheel(MouseButton::WheelUp),
            );
        }
        assert_eq!(ui.scroll, Some(0));
        assert_eq!(ui.selected, selected);
    }

    #[test]
    fn mouse_tabs_use_painted_labels_and_ignore_gaps_and_releases() {
        for ascii in [false, true] {
            let screen = TextBuffer::new(120, 20);
            let mut ui = ui(View::Mine);
            let event = |x, release| {
                let mouse =
                    uncurses::event::Mouse::new(x, 0, MouseButton::Left, KeyModifiers::empty());
                if release {
                    Event::MouseRelease(mouse)
                } else {
                    Event::MouseClick(mouse)
                }
            };
            ui.mouse_event(&screen, &Sections::EMPTY, "", None, ascii, &event(9, true));
            assert_eq!(ui.view, View::Mine);
            ui.mouse_event(&screen, &Sections::EMPTY, "", None, ascii, &event(8, false));
            assert_eq!(ui.view, View::Mine);
            ui.selected = Some(0);
            ui.mouse_event(
                &screen,
                &Sections::EMPTY,
                "",
                None,
                ascii,
                &event(10, false),
            );
            assert_eq!(ui.view, View::Reviews);
            assert_eq!(ui.selected, None);
            assert_eq!(ui.scroll, None);
            ui.mouse_event(&screen, &Sections::EMPTY, "", None, ascii, &event(0, false));
            assert_eq!(ui.view, View::Mine);
        }
    }

    #[test]
    fn page_keys_scroll_in_normal_and_search_modes() {
        use uncurses::event::Key;

        for (code, down) in [(KeyCode::PageUp, false), (KeyCode::PageDown, true)] {
            let event = Event::KeyPress(Key::new(code, KeyModifiers::empty()).normalized());
            assert!(matches!(classify(&event), Action::Scroll(value) if value == down));
            assert_eq!(classify_search(&event), SearchAction::Scroll(down));
        }
    }

    #[test]
    fn page_scrolling_reaches_every_help_line_and_empty_section_without_targets() {
        let sections = Sections {
            prs: Some(vec![]),
            queue: Some(vec![]),
            merged: Some(vec![]),
            reviews: Some(vec![]),
            reviewed_merged: Some(vec![]),
            ..Sections::EMPTY
        };
        for view in [View::Mine, View::Reviews] {
            let mut ui = ui(view);
            ui.show_help = true;
            assert_eq!(
                nav::targets_visible(view, &sections, "", Visibility::all(&sections)).len(),
                0
            );
            let screen = TextBuffer::new(120, 6);
            let (body, height, selected, _, _) =
                watch_body(&screen, &sections, &ui, &Changes::default(), &[], true);
            let expected = body.display_with(Profile::Disabled).to_string();
            let mut seen = HashSet::new();
            for _ in 0..height {
                let frame = watch_frame(&sections, &ui, 120, 6);
                let text = frame.display_with(Profile::Disabled).to_string();
                let lines: Vec<_> = text.lines().collect();
                assert!(lines.last().unwrap().starts_with("^R refresh"));
                seen.extend(
                    lines[..lines.len() - 1]
                        .iter()
                        .map(|line| line.trim_end().to_string()),
                );
                ui.scroll_page(true, height, 5, selected);
            }
            for line in expected.lines().filter(|line| !line.trim().is_empty()) {
                assert!(seen.contains(line.trim_end()), "{view:?}: missing {line:?}");
            }
            for _ in 0..height {
                ui.scroll_page(false, height, 5, selected);
            }
            assert_eq!(ui.scroll, Some(0));
            let frame = watch_frame(&sections, &ui, 120, 6);
            assert!(
                frame
                    .display_with(Profile::Disabled)
                    .to_string()
                    .starts_with("Help")
            );
        }
    }

    #[test]
    fn manual_scroll_preserves_selection_and_clamps_after_resize() {
        let sections = Sections {
            prs: Some(vec![queued_open_row(1)]),
            ..Sections::EMPTY
        };
        let mut ui = ui(View::Mine);
        ui.show_help = true;
        ui.selected = Some(0);
        ui.scroll_page(true, 40, 5, Some(20));
        assert_eq!(ui.scroll, Some(23));
        assert_eq!(ui.selected, Some(0));
        ui.scroll_page(false, 10, 5, Some(20));
        assert_eq!(ui.scroll, Some(0));
        let frame = watch_frame(&sections, &ui, 120, 6);
        assert!(
            frame
                .display_with(Profile::Disabled)
                .to_string()
                .starts_with("Help")
        );
        assert_eq!(ui.selected, Some(0));
        ui.scroll_page(true, 3, 5, None);
        assert_eq!(ui.scroll, Some(0));
        ui.scroll_page(true, 3, 0, None);
        assert_eq!(ui.scroll, Some(1));
    }

    #[test]
    fn short_terminals_keep_every_section_available_for_scrolling() {
        let sections = Sections {
            prs: Some(
                (1..=30)
                    .map(|n| {
                        let mut row = queued_open_row(n);
                        row.queue = None;
                        row
                    })
                    .collect(),
            ),
            queue: Some((1..=30).map(|n| queue_row(n, false, false)).collect()),
            merged: Some((31..=40).map(merged_row).collect()),
            commits: Some(commits::CommitStats {
                available: true,
                upcoming: Some(commits::Bucket {
                    count: commits::Count {
                        mine: 1,
                        capped: false,
                    },
                    url: "https://compare/upcoming".into(),
                }),
                releases: vec![],
            }),
            ..Sections::EMPTY
        };
        let mut ui = ui(View::Mine);
        let targets = nav::targets_visible(ui.view, &sections, "", Visibility::all(&sections));
        assert_eq!(targets.len(), 71);
        for (index, target) in targets.iter().enumerate() {
            ui.selected = nav::moved(nav::Move::Down, ui.selected, targets.len(), 4);
            assert_eq!(ui.selected, Some(index));
            let frame = watch_frame(&sections, &ui, 120, 8);
            let text = frame.display_with(Profile::Disabled).to_string();
            assert!(text.contains(target.title), "{index}: {text}");
            assert!(text.lines().last().unwrap().contains("^R refresh"));
            assert!(!text.contains("Terminal too small"));
            assert!(!text.contains("hidden"));
        }
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
    fn scrolling_queue_accounts_for_search_status_and_selection() {
        let sections = Sections {
            queue: Some((0..=40).map(|n| queue_row(n, n == 0, n == 0)).collect()),
            ..Sections::EMPTY
        };
        let ui = Ui {
            search: "other".into(),
            searching: true,
            selected: Some(39),
            ..ui(View::Mine)
        };
        let mut filtered = None;
        let shown = ui.shown(&sections, &mut filtered);
        let footer = Some(("5m", true));
        let status = "error: offline";
        let visible = Visibility::all(shown);
        let expected: Vec<_> = (1..=40).map(|n| format!("https://queue/{n}")).collect();
        let targets = nav::targets_visible(View::Mine, shown, &ui.search, visible);
        assert_eq!(nav::urls(targets), expected);
        let section = nav::section_at_visible(View::Mine, shown, &ui.search, 39, visible);
        assert_eq!(nav::urls(section), expected);
        assert_eq!(
            nav::target_index(View::Mine, shown, &ui.search, visible, "https://queue/40"),
            Some(39)
        );
        assert_eq!(
            nav::target_index(View::Mine, shown, &ui.search, visible, "https://queue/7"),
            Some(6)
        );

        let mut screen = TextBuffer::new(120, 8);
        let caret = paint_watch_frame(
            &mut screen,
            shown,
            &ui,
            &Changes::default(),
            status,
            footer,
            true,
        );
        let text = screen.display_with(Profile::Disabled).to_string();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 8);
        assert!(lines[..3].iter().any(|line| line.contains("queue-40")));
        assert!(lines[3].contains("/other  (40 matches)"));
        assert!(lines[5].contains(status));
        assert!(lines[7].contains("^R refreshing"));
        assert_eq!(caret, Some(Position::new(6, 3)));
        let selected = lines
            .iter()
            .position(|line| line.contains("queue-40"))
            .unwrap() as u16;
        assert_eq!(
            screen.cell(Position::new(0, selected)).unwrap().style.bg,
            Some(status::SURFACE)
        );
    }

    #[test]
    fn queue_order_and_targets_do_not_depend_on_terminal_size() {
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

        for height in 2..=20 {
            let mut ui = ui(View::Mine);
            let targets = nav::targets_visible(ui.view, &sections, "", Visibility::all(&sections));
            assert_eq!(
                nav::urls(targets.clone()),
                [
                    "https://queue/1",
                    "https://queue/2",
                    "https://queue/3",
                    "https://queue/4",
                    "https://queue/5"
                ]
            );
            for (index, target) in targets.iter().enumerate() {
                ui.selected = Some(index);
                let canvas = watch_frame(&sections, &ui, 120, height);
                let text = canvas.display_with(Profile::Disabled).to_string();
                assert!(
                    text.contains(target.title),
                    "height={height}, index={index}: {text}"
                );
                assert!(!text.contains("+ resize for more"));
            }
        }
    }

    #[test]
    fn queue_dedup_remains_consistent_in_short_frames() {
        let sections = Sections {
            prs: Some((1..=4).map(queued_open_row).collect()),
            queue: Some((1..=4).map(|n| queue_row(n, true, false)).collect()),
            ..Sections::EMPTY
        };
        let visible = Visibility::all(&sections);
        assert_eq!(
            nav::targets_visible(View::Mine, &sections, "", visible).len(),
            4
        );
        let ui = Ui {
            selected: Some(3),
            ..ui(View::Mine)
        };
        let canvas = watch_frame(&sections, &ui, 120, 8);
        let text = canvas.display_with(Profile::Disabled).to_string();
        assert!(text.contains("queue-4"));
        assert!(!text.contains("open-queued-"));
    }

    #[test]
    fn help_scrolls_with_the_body_without_hiding_sections() {
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
        let canvas = watch_frame(&sections, &ui, 120, 8);
        let text = canvas.display_with(Profile::Disabled).to_string();
        assert!(text.starts_with("Help"));
        assert!(text.lines().last().unwrap().contains("^R refresh"));
        assert!(!text.contains("+ resize for more"));
        let canvas = watch_frame(&sections, &ui, 120, 40);
        let text = canvas.display_with(Profile::Disabled).to_string();
        for header in [
            "Help",
            "My open PRs",
            "Merge Queue",
            "My merged PRs",
            "Commit stats unavailable.",
        ] {
            assert!(text.contains(header), "{header}: {text}");
        }
    }

    #[test]
    fn disabled_queue_keeps_queued_pr_in_open_section() {
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
                created_at: None,
            }]),
            ..Sections::EMPTY
        };
        let ui = Ui {
            selected: Some(0),
            ..ui(View::Mine)
        };
        let canvas = watch_frame(&sections, &ui, 120, 3);
        let text = canvas.display_with(Profile::Disabled).to_string();
        assert!(text.contains("queued"));
        assert_eq!(
            ui.copy_text(&sections, Visibility::all(&sections), "{url}", false),
            Some(("https://pr/1".into(), 1))
        );
    }

    #[test]
    fn reviews_scroll_to_every_open_and_merged_row() {
        let sections = Sections {
            reviews: Some((1..=30).map(review_row).collect()),
            reviewed_merged: Some(
                (31..=40)
                    .map(|n| reviews::ReviewedMergedRow {
                        number: n,
                        title: format!("reviewed-{n:02}"),
                        branch: String::new(),
                        author: "other".into(),
                        url: format!("https://reviewed/{n}"),
                        merged_at: None,
                    })
                    .collect(),
            ),
            ..Sections::EMPTY
        };
        let mut ui = ui(View::Reviews);
        let visible = Visibility::all(&sections);
        let targets = nav::targets_visible(ui.view, &sections, "", visible);
        assert_eq!(targets.len(), 40);
        for movement in [nav::Move::Down, nav::Move::Up] {
            ui.selected = if movement == nav::Move::Up {
                Some(39)
            } else {
                None
            };
            for _ in 0..40 {
                ui.selected = nav::moved(movement, ui.selected, targets.len(), 4);
                let selected = ui.selected.unwrap();
                let canvas = watch_frame(&sections, &ui, 120, 8);
                let text = canvas.display_with(Profile::Disabled).to_string();
                assert!(text.contains(targets[selected].title), "{selected}: {text}");
                assert!(text.lines().last().unwrap().contains("^R refresh"));
                assert_eq!(
                    ui.copy_text(&sections, visible, "{url}", false),
                    Some((targets[selected].url.into(), 1))
                );
            }
        }
        ui.selected = nav::moved(nav::Move::Bottom, None, targets.len(), 4);
        let mut opened = Vec::new();
        ui.open_links(&sections, visible, true, |url| {
            opened.push(url.to_string());
            Ok(())
        })
        .unwrap();
        assert_eq!(opened.len(), 10);
        assert_eq!(opened.last().unwrap(), "https://reviewed/40");
        assert_eq!(
            ui.copy_text(&sections, visible, "{url}", true).unwrap().1,
            10
        );
        ui.search = "reviewed-40".into();
        ui.selected = Some(0);
        let mut filtered = None;
        let shown = ui.shown(&sections, &mut filtered);
        let canvas = watch_frame(shown, &ui, 120, 8);
        let text = canvas.display_with(Profile::Disabled).to_string();
        assert!(text.contains("reviewed-40"));
        assert!(text.contains("(1 match)"));
    }

    #[test]
    fn narrow_and_short_frames_clip_instead_of_rejecting_the_dashboard() {
        let sections = Sections {
            queue: Some((1..=40).map(|n| queue_row(n, false, false)).collect()),
            ..Sections::EMPTY
        };
        let ui = Ui {
            selected: Some(39),
            ..ui(View::Mine)
        };
        for width in [1, 4, 12, 23, 24, 40] {
            for height in [1, 2, 8, 40] {
                let canvas = watch_frame(&sections, &ui, width, height);
                let text = canvas.display_with(Profile::Disabled).to_string();
                assert!(
                    !text.contains("Terminal too small"),
                    "{width}x{height}: {text}"
                );
                assert_eq!(canvas.height(), height);
                assert!(
                    text.lines().last().unwrap().starts_with(['^', '+']),
                    "{width}x{height}: {text}"
                );
                if height >= 2 {
                    assert!(
                        (0..height - 1).any(|y| canvas.cell(Position::new(0, y)).unwrap().style.bg
                            == Some(status::SURFACE)),
                        "{width}x{height}: {text}"
                    );
                }
            }
            let mut canvas = TextBuffer::new(width, 64);
            let (used, _) = paint_dashboard(
                &mut canvas,
                &sections,
                &ui,
                &Changes::default(),
                RenderMessages {
                    warnings: &[],
                    status: "",
                },
                None,
                true,
            );
            assert_eq!(used, 43);
            let text = canvas.display_with(Profile::Disabled).to_string();
            assert!(text.starts_with(&"Merge Queue"[..usize::from(width).min(11)]));
            assert!(!text.contains("Terminal too small"));
        }
    }

    #[test]
    fn one_shot_output_keeps_all_rows_and_help_at_full_height() {
        let sections = Sections {
            queue: Some((1..=40).map(|n| queue_row(n, false, false)).collect()),
            merged: Some((41..=50).map(merged_row).collect()),
            reviews: Some((1..=40).map(review_row).collect()),
            ..Sections::EMPTY
        };
        for view in [View::Mine, View::Reviews] {
            let ui = Ui {
                show_help: true,
                ..ui(view)
            };
            let targets = nav::targets_visible(view, &sections, "", Visibility::all(&sections));
            let text = render_to_string(
                &sections,
                &ui,
                &Changes::default(),
                None,
                true,
                Profile::Disabled,
            );
            for target in targets {
                assert!(text.contains(target.title), "{view:?}: {}", target.title);
            }
            assert!(text.contains("Help"));
            assert!(text.contains("g/G first/last"));
            assert!(!text.contains("hidden"));
            assert!(!text.contains("Terminal too small"));
            assert!(text.lines().count() > 40);
        }
    }

    #[test]
    fn resize_refresh_and_filter_keep_selection_attached_to_the_url() {
        let mut sections = Sections {
            merged: Some((1..=30).map(merged_row).collect()),
            reviews: Some((1..=30).map(review_row).collect()),
            ..Sections::EMPTY
        };
        for view in [View::Mine, View::Reviews] {
            let mut ui = Ui {
                selected: Some(29),
                ..ui(view)
            };
            let url = ui.selected_url(&sections).unwrap();
            let expected_targets =
                nav::targets_visible(view, &sections, "", Visibility::all(&sections));
            for (width, height) in [(120, 40), (120, 8), (12, 2), (1, 2), (80, 16)] {
                ui.restore_selection(&sections, Some(&url));
                let canvas = watch_frame(&sections, &ui, width, height);
                assert_eq!(ui.selected, Some(29));
                assert_eq!(ui.selected_url(&sections).as_deref(), Some(url.as_str()));
                assert_eq!(
                    nav::targets_visible(view, &sections, "", Visibility::all(&sections)),
                    expected_targets
                );
                assert!(
                    (0..height - 1).any(|y| canvas.cell(Position::new(0, y)).unwrap().style.bg
                        == Some(status::SURFACE))
                );
            }
            match view {
                View::Mine => sections.merged.as_mut().unwrap().insert(0, merged_row(99)),
                View::Reviews => sections.reviews.as_mut().unwrap().insert(0, review_row(99)),
            }
            ui.restore_selection(&sections, Some(&url));
            assert_eq!(ui.selected, Some(30));
            assert_eq!(
                ui.copy_text(&sections, Visibility::all(&sections), "{url}", false),
                Some((url.clone(), 1))
            );
            let canvas = watch_frame(&sections, &ui, 120, 8);
            assert!(
                canvas
                    .display_with(Profile::Disabled)
                    .to_string()
                    .contains("-30")
            );

            ui.search = "30".into();
            ui.selected = None;
            let mut filtered = None;
            let shown = ui.shown(&sections, &mut filtered);
            assert_eq!(
                nav::targets_visible(view, shown, &ui.search, Visibility::all(shown)).len(),
                1
            );
            ui.restore_selection(&sections, Some(&url));
            assert_eq!(ui.selected, Some(0));
            assert_eq!(ui.selected_url(&sections).as_deref(), Some(url.as_str()));
            ui.search = "no matches".into();
            ui.restore_selection(&sections, Some(&url));
            assert_eq!(ui.selected, None);
        }
    }

    #[test]
    fn movement_keys_reach_both_ends_and_half_pages_in_both_views() {
        use uncurses::event::Key;

        let sections = Sections {
            merged: Some((1..=30).map(merged_row).collect()),
            reviews: Some((1..=30).map(review_row).collect()),
            ..Sections::EMPTY
        };
        for view in [View::Mine, View::Reviews] {
            let mut ui = ui(view);
            for (code, modifiers, expected) in [
                (KeyCode::Char('G'), KeyModifiers::empty(), 29),
                (KeyCode::Char('g'), KeyModifiers::empty(), 0),
                (KeyCode::Char('d'), KeyModifiers::CTRL, 4),
                (KeyCode::Char('u'), KeyModifiers::CTRL, 0),
                (KeyCode::Down, KeyModifiers::empty(), 1),
                (KeyCode::Up, KeyModifiers::empty(), 0),
                (KeyCode::Char('j'), KeyModifiers::empty(), 1),
                (KeyCode::Char('k'), KeyModifiers::empty(), 0),
            ] {
                let event = Event::KeyPress(Key::new(code, modifiers).normalized());
                let Action::Move(movement) = classify(&event) else {
                    panic!("{event:?} is not a movement");
                };
                ui.selected = nav::moved(movement, ui.selected, 30, 4);
                assert_eq!(ui.selected, Some(expected), "{view:?}: {event:?}");
                let canvas = watch_frame(&sections, &ui, 120, 8);
                let text = canvas.display_with(Profile::Disabled).to_string();
                let targets = nav::targets_visible(view, &sections, "", Visibility::all(&sections));
                assert!(
                    text.contains(targets[expected].title),
                    "{view:?}: {event:?}: {text}"
                );
            }
        }
    }

    #[test]
    fn search_status_and_footer_stay_pinned_in_both_views() {
        let sections = Sections {
            queue: Some((1..=40).map(|n| queue_row(n, false, false)).collect()),
            reviews: Some((1..=40).map(review_row).collect()),
            ..Sections::EMPTY
        };
        for view in [View::Mine, View::Reviews] {
            let ui = Ui {
                search: "other".into(),
                searching: true,
                selected: Some(39),
                ..ui(view)
            };
            let mut filtered = None;
            let shown = ui.shown(&sections, &mut filtered);
            for height in [6, 8, 20] {
                let mut canvas = TextBuffer::new(120, height);
                let caret = paint_watch_frame(
                    &mut canvas,
                    shown,
                    &ui,
                    &Changes::default(),
                    "error: offline",
                    Some(("5m", true)),
                    true,
                );
                let text = canvas.display_with(Profile::Disabled).to_string();
                let lines: Vec<_> = text.lines().collect();
                assert!(lines[usize::from(height - 5)].contains("/other  (40 matches)"));
                assert!(lines[usize::from(height - 3)].contains("error: offline"));
                assert!(lines[usize::from(height - 1)].contains("^R refreshing"));
                assert_eq!(caret, Some(Position::new(6, height - 5)));
                assert!(
                    (0..height - 5).any(|y| canvas.cell(Position::new(0, y)).unwrap().style.bg
                        == Some(status::SURFACE))
                );
            }
            for width in [1, 4, 12] {
                for height in 1..=8 {
                    let mut canvas = TextBuffer::new(width, height);
                    let caret = paint_watch_frame(
                        &mut canvas,
                        shown,
                        &ui,
                        &Changes::default(),
                        "error: offline",
                        Some(("5m", false)),
                        true,
                    );
                    if let Some(caret) = caret {
                        assert!(
                            caret.x < width && caret.y < height,
                            "{width}x{height}: {caret:?}"
                        );
                    }
                    assert_eq!(caret.is_some(), height >= 5);
                }
            }
        }
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
            created_at: None,
        };
        let sections = Sections {
            prs: Some(vec![pr(1), pr(2)]),
            ..Sections::EMPTY
        };

        // The rows carrying the selection background, and their text.
        let highlighted = |ui: &Ui| -> Vec<String> {
            let w = render::OUTPUT_WIDTH as u16;
            let mut canvas = TextBuffer::new(w, 64);
            paint_dashboard(
                &mut canvas,
                &sections,
                ui,
                &Changes::default(),
                RenderMessages {
                    warnings: &[],
                    status: "",
                },
                None,
                true,
            );
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
        assert_eq!(highlighted(&ui(View::Mine)), Vec::<String>::new());

        // Selecting the second row highlights exactly that row, whole: the bar
        // reaches the leading marker column, which the caret used to occupy.
        let sel = highlighted(&Ui {
            selected: Some(1),
            ..ui(View::Mine)
        });
        assert_eq!(sel.len(), 1, "expected one highlighted row, got {sel:?}");
        assert!(sel[0].contains("#2"), "wrong row highlighted: {:?}", sel[0]);
    }

    const RELEASES: &str = "GET /repos/o/r/releases?per_page=50&page=1";
    const GRAPHQL: &str = "POST /graphql";

    fn release(tag: &str) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "published_at": "2026-01-01T00:00:00Z",
        })
    }

    fn commit(login: &str, message: &str) -> serde_json::Value {
        serde_json::json!({ "author": { "login": login }, "commit": { "message": message } })
    }

    /// One GraphQL answer for the PR queries: each node parses as an open,
    /// merged, or review PR, and the queue is null.
    fn graphql(numbers: &[i64]) -> (String, String) {
        let nodes: Vec<_> = numbers
            .iter()
            .map(|n| {
                serde_json::json!({
                    "number": n,
                    "title": format!("PR {n}"),
                    "url": format!("https://github.com/o/r/pull/{n}"),
                    "mergedAt": format!("2026-01-0{n}T00:00:00Z"),
                    "isDraft": false,
                    "reviewThreads": { "totalCount": 0, "nodes": [] },
                    "commits": { "nodes": [] },
                    "reviews": { "nodes": [] },
                })
            })
            .collect();
        let data = serde_json::json!({ "data": {
            "search": { "nodes": nodes, "pageInfo": { "hasNextPage": false, "endCursor": null } },
            "repository": { "mergeQueue": null },
            "requested": { "nodes": nodes },
            "reviewed": { "nodes": [] },
            "reviewedMerged": { "nodes": nodes },
        }});
        (GRAPHQL.to_string(), data.to_string())
    }

    /// Releases v2 and v1: PR 3 is unreleased, PR 2 shipped in v2, PR 1 in v1.
    fn tagged_with_merged(numbers: &[i64]) -> Vec<(String, String)> {
        let routes = [
            (RELEASES, serde_json::json!([release("v2"), release("v1")])),
            (
                "GET /repos/o/r/compare/v2...main",
                serde_json::json!({ "total_commits": 2, "commits": [
                    commit("me", "feat: three (#3)"),
                    commit("other", "fix: theirs (#9)"),
                ]}),
            ),
            (
                "GET /repos/o/r/compare/v1...v2",
                serde_json::json!({ "total_commits": 1, "commits": [commit("me", "feat: two (#2)")] }),
            ),
            (
                "GET /repos/o/r/commits?sha=v1&author=me&per_page=100&page=1",
                serde_json::json!([commit("me", "feat: one (#1)")]),
            ),
        ];
        routes
            .into_iter()
            .map(|(route, body)| (route.to_string(), body.to_string()))
            .chain([graphql(numbers)])
            .collect()
    }

    fn tagged() -> Vec<(String, String)> {
        tagged_with_merged(&[3, 2, 1])
    }

    fn tagged_watch() -> Vec<(String, String)> {
        tagged_watch_with_merged(&[3, 2, 1])
    }

    fn tagged_watch_with_merged(numbers: &[i64]) -> Vec<(String, String)> {
        let mut routes = tagged_with_merged(numbers);
        let body = &mut routes
            .iter_mut()
            .find(|(route, _)| route == GRAPHQL)
            .unwrap()
            .1;
        let mut response: serde_json::Value = serde_json::from_str(body).unwrap();
        let nodes = response["data"]["search"]["nodes"].clone();
        response["data"]["mineMerged"] = serde_json::json!({ "nodes": nodes });
        *body = response.to_string();
        routes
    }

    /// No releases, so the upcoming count pages through every commit of mine.
    fn untagged() -> Vec<(String, String)> {
        let page = |n| format!("GET /repos/o/r/commits?sha=main&author=me&per_page=100&page={n}");
        vec![
            (RELEASES.to_string(), "[]".to_string()),
            (
                page(1),
                serde_json::Value::from(vec![commit("me", "chore: old"); 100]).to_string(),
            ),
            (
                page(2),
                serde_json::json!([commit("me", "feat: one (#1)")]).to_string(),
            ),
            graphql(&[1]),
        ]
    }

    fn fetch_offline(args: &[&str], routes: Vec<(String, String)>) -> (Sections, Vec<String>) {
        let cli = Cli::try_parse_from(args).unwrap();
        let (client, fake) = github::fake::client(routes);
        let repo = Repo::parse("o/r").unwrap();
        let outcome = fetch(
            &cli,
            &client,
            &repo,
            "me",
            &mut Some("main".to_string()),
            true,
            false,
        );
        assert!(outcome.issues.is_empty());
        (outcome.sections, fake.requests())
    }

    #[test]
    fn mine_partial_graphql_error_keeps_rows_and_warns_without_extra_requests() {
        let cli = Cli::try_parse_from(["prowl", "--only", "mine"]).unwrap();
        let repo = Repo::parse("o/r").unwrap();
        let body = include_str!("../tests/fixtures/mine_no_rollup.json");
        let (client, fake) = github::fake::client([(GRAPHQL.to_string(), body.to_string())]);

        let outcome = fetch(&cli, &client, &repo, "me", &mut None, true, false);

        assert_eq!(fake.requests(), [GRAPHQL]);
        assert_eq!(
            outcome.sections.prs.as_ref().unwrap()[0].number,
            123,
            "usable data must survive the GraphQL error"
        );
        assert!(!outcome.has_errors());
        assert!(
            outcome.status().contains("warning: My PRs partial data"),
            "{}",
            outcome.status()
        );
        let warnings = outcome.warnings();
        let once = render_to_string_with_warnings(
            &outcome.sections,
            &Ui::once(&cli),
            &Changes::default(),
            RenderMessages {
                warnings: &warnings,
                status: &outcome.status(),
            },
            None,
            true,
            Profile::Disabled,
        );
        assert!(
            once.lines()
                .any(|line| line.contains("My open PRs") && line.contains("partial data")),
            "{once}"
        );
        let mut watch = TextBuffer::new(120, 12);
        let mut ui = Ui::once(&cli);
        ui.show_help = false;
        paint_watch_frame_with_warnings(
            &mut watch,
            &outcome.sections,
            &ui,
            &Changes::default(),
            &warnings,
            &outcome.status(),
            None,
            true,
        );
        let watch = watch.display_with(Profile::Disabled).to_string();
        assert!(
            watch
                .lines()
                .any(|line| line.contains("My open PRs") && line.contains("partial data")),
            "{watch}"
        );
    }

    #[test]
    fn clean_mine_graphql_response_has_no_partial_warning() {
        let cli = Cli::try_parse_from(["prowl", "--only", "mine"]).unwrap();
        let repo = Repo::parse("o/r").unwrap();
        let mut response: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/mine_no_rollup.json")).unwrap();
        response.as_object_mut().unwrap().remove("errors");
        let (client, fake) = github::fake::client([(GRAPHQL.to_string(), response.to_string())]);

        let outcome = fetch(&cli, &client, &repo, "me", &mut None, true, false);

        assert!(outcome.issues.is_empty());
        assert_eq!(fake.requests(), [GRAPHQL]);
        let output = render_to_string_with_warnings(
            &outcome.sections,
            &Ui::once(&cli),
            &Changes::default(),
            RenderMessages {
                warnings: &outcome.warnings(),
                status: &outcome.status(),
            },
            None,
            true,
            Profile::Disabled,
        );
        assert!(!output.contains("partial data"), "{output}");
    }

    #[test]
    fn mine_partial_graphql_error_does_not_hide_a_typed_parse_failure() {
        let cli = Cli::try_parse_from(["prowl", "--only", "mine"]).unwrap();
        let repo = Repo::parse("o/r").unwrap();
        let mut response: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/mine_no_rollup.json")).unwrap();
        response["data"]["search"]
            .as_object_mut()
            .unwrap()
            .remove("pageInfo");
        let (client, fake) = github::fake::client([(GRAPHQL.to_string(), response.to_string())]);

        let outcome = fetch(&cli, &client, &repo, "me", &mut None, true, false);

        assert!(outcome.has_errors());
        assert!(outcome.sections.prs.is_none());
        assert!(outcome.status().contains("unavailable"));
        assert_eq!(fake.requests(), [GRAPHQL]);
    }

    #[test]
    fn each_section_failure_preserves_other_fetched_sections() {
        let cli = Cli::try_parse_from(["prowl"]).unwrap();
        let repo = Repo::parse("o/r").unwrap();
        for call in 1..=3 {
            let (client, fake) = github::fake::client(tagged_watch());
            fake.set_response_on_call(
                GRAPHQL,
                call,
                500,
                &[],
                r#"{"message":"injected section failure"}"#,
            );
            let outcome = fetch(
                &cli,
                &client,
                &repo,
                "me",
                &mut Some("main".to_string()),
                true,
                true,
            );
            let requests = fake.requests();
            let posts = requests.iter().filter(|r| r.as_str() == GRAPHQL).count();
            assert_eq!(posts, 3, "GraphQL call {call}: {requests:?}");
            assert_eq!(outcome.issues.len(), if call == 1 { 3 } else { 1 });
            let expected_scopes = match call {
                1 => vec![
                    FetchScope::Merged,
                    FetchScope::Reviews,
                    FetchScope::ReviewedMerged,
                ],
                2 => vec![FetchScope::Queue],
                3 => vec![FetchScope::Mine],
                _ => unreachable!(),
            };
            assert_eq!(
                outcome
                    .issues
                    .iter()
                    .map(|issue| issue.scope)
                    .collect::<Vec<_>>(),
                expected_scopes
            );
            assert!(
                outcome
                    .issues
                    .iter()
                    .all(|issue| issue.message.contains("500"))
            );
            assert!(outcome.issues.iter().all(|issue| !issue.stale));
            assert_eq!(outcome.status().lines().count(), outcome.issues.len());
            match call {
                1 => {
                    assert!(outcome.sections.merged.is_none());
                    assert!(outcome.sections.reviews.is_none());
                    assert!(outcome.sections.reviewed_merged.is_none());
                }
                2 => assert!(outcome.sections.queue.is_none()),
                3 => assert!(outcome.sections.prs.is_none()),
                _ => unreachable!(),
            }
            assert_eq!(outcome.sections.merged.is_some(), call != 1);
            assert_eq!(outcome.sections.queue.is_some(), call != 2);
            assert_eq!(outcome.sections.prs.is_some(), call != 3);
            assert_eq!(outcome.sections.reviews.is_some(), call != 1);
            assert_eq!(outcome.sections.reviewed_merged.is_some(), call != 1);
            assert!(outcome.sections.commits.is_some());
        }
    }

    #[test]
    fn cold_shipment_failure_is_reported_without_empty_stats() {
        let cli = Cli::try_parse_from(["prowl", "--only", "shipments"]).unwrap();
        let repo = Repo::parse("o/r").unwrap();
        let (client, fake) = github::fake::client(tagged());
        fake.set_response(
            RELEASES,
            500,
            &[],
            r#"{"message":"injected release failure"}"#,
        );

        let outcome = fetch(
            &cli,
            &client,
            &repo,
            "me",
            &mut Some("main".to_string()),
            true,
            false,
        );

        assert!(
            outcome
                .sections
                .commits
                .as_ref()
                .is_some_and(|stats| !stats.available)
        );
        assert_eq!(outcome.issues.len(), 1);
        assert_eq!(outcome.issues[0].scope, FetchScope::Shipments);
        assert!(outcome.status().contains("My Shipments unavailable"));
    }

    #[test]
    fn hidden_release_failure_is_reported_without_hiding_merged_rows() {
        let cli = Cli::try_parse_from(["prowl", "--only", "merged"]).unwrap();
        let repo = Repo::parse("o/r").unwrap();
        let (client, fake) = github::fake::client(tagged());
        fake.set_response(
            RELEASES,
            500,
            &[],
            r#"{"message":"injected release failure"}"#,
        );

        let outcome = fetch(
            &cli,
            &client,
            &repo,
            "me",
            &mut Some("main".to_string()),
            true,
            false,
        );

        assert_eq!(fake.requests(), [GRAPHQL, RELEASES]);
        assert_eq!(outcome.issues.len(), 1);
        assert_eq!(outcome.issues[0].scope, FetchScope::ReleaseMetadata);
        assert!(
            outcome
                .status()
                .contains("Merged release metadata unavailable")
        );
        assert_eq!(
            shipped(&outcome.sections),
            [(3, None), (2, None), (1, None)]
        );
        let rows = outcome.sections.merged.as_ref().unwrap();
        assert!(rows.iter().all(|row| row.release_unknown));
        let table = merged::to_table(rows, true, &HashSet::new(), false);
        assert!(table.rows.iter().all(|row| row[4].text == "?"));
    }

    #[test]
    fn partial_release_failure_keeps_successes_and_marks_unknown_rows() {
        let cli = Cli::try_parse_from(["prowl"]).unwrap();
        let repo = Repo::parse("o/r").unwrap();
        let (client, fake) = github::fake::client(tagged_watch());
        fake.set_response(
            "GET /repos/o/r/compare/v1...v2",
            500,
            &[],
            r#"{"message":"injected release failure"}"#,
        );

        let outcome = fetch(
            &cli,
            &client,
            &repo,
            "me",
            &mut Some("main".to_string()),
            true,
            true,
        );

        let stats = outcome.sections.commits.as_ref().unwrap();
        assert!(stats.available);
        assert_eq!(
            stats
                .releases
                .iter()
                .map(|release| release.count.is_some())
                .collect::<Vec<_>>(),
            [false, true]
        );
        assert_eq!(
            shipped(&outcome.sections),
            [(3, None), (2, None), (1, Some("v1".into()))]
        );
        assert!(outcome.status().contains("My Shipments stale"));
        assert!(outcome.status().contains("Merged release metadata stale"));
        let rows = outcome.sections.merged.as_ref().unwrap();
        assert!(rows[0].release_unknown);
        assert!(rows[1].release_unknown);
        assert!(!rows[2].release_unknown);
        let table = merged::to_table(rows, true, &HashSet::new(), false);
        assert_eq!(table.rows[0][4].text, "?");
        assert_eq!(table.rows[1][4].text, "?");
        assert_eq!(table.rows[2][4].text, "v1");
        assert!(
            !outcome.issues.is_empty(),
            "partial outcomes must not be cached"
        );
        assert!(
            fake.requests()
                .contains(&"GET /repos/o/r/commits?sha=v1&author=me&per_page=100&page=1".into())
        );
    }

    #[test]
    fn hidden_release_range_failure_keeps_known_annotations_and_marks_unknown_rows() {
        let cli = Cli::try_parse_from(["prowl", "--only", "merged"]).unwrap();
        let repo = Repo::parse("o/r").unwrap();
        let (client, fake) = github::fake::client(tagged());
        fake.set_response(
            "GET /repos/o/r/compare/v1...v2",
            500,
            &[],
            r#"{"message":"injected release failure"}"#,
        );

        let outcome = fetch(
            &cli,
            &client,
            &repo,
            "me",
            &mut Some("main".to_string()),
            true,
            false,
        );

        assert_eq!(
            fake.requests(),
            [
                GRAPHQL,
                RELEASES,
                "GET /repos/o/r/compare/v1...v2",
                "GET /repos/o/r/commits?sha=v1&author=me&per_page=100&page=1",
            ]
        );
        assert!(outcome.status().contains("Merged release metadata stale"));
        let rows = outcome.sections.merged.as_ref().unwrap();
        assert!(rows[0].release_unknown);
        assert!(rows[1].release_unknown);
        assert!(!rows[2].release_unknown);
        assert_eq!(rows[2].release.as_ref().unwrap().tag, "v1");
    }

    #[test]
    fn failed_refresh_keeps_stale_sections_and_updates_successful_ones() {
        let cli = Cli::try_parse_from(["prowl"]).unwrap();
        let repo = Repo::parse("o/r").unwrap();
        let (client, _) = github::fake::client(tagged_watch());
        let mut branch = Some("main".to_string());
        let baseline = fetch(&cli, &client, &repo, "me", &mut branch, true, true);
        assert!(baseline.issues.is_empty());
        let mut previous = baseline.sections;
        previous.queue_next_eta = Some(42);
        for row in previous.prs.as_mut().unwrap() {
            row.title = "old snapshot".to_string();
        }

        let (client, fake) = github::fake::client(tagged_watch());
        fake.set_response_on_call(
            GRAPHQL,
            2,
            500,
            &[],
            r#"{"message":"injected queue failure"}"#,
        );
        let current =
            fetch(&cli, &client, &repo, "me", &mut branch, true, true).reconcile(Some(previous));

        assert_eq!(current.issues.len(), 1);
        assert_eq!(current.issues[0].scope, FetchScope::Queue);
        assert!(current.issues[0].stale);
        assert_eq!(current.sections.queue_next_eta, Some(42));
        assert!(
            current
                .sections
                .prs
                .as_ref()
                .unwrap()
                .iter()
                .all(|row| row.title != "old snapshot"),
            "successful sections must use current data"
        );
        assert!(current.status().contains("Merge Queue stale"));
    }

    #[test]
    fn shipment_failure_keeps_available_stats_and_release_annotations() {
        let cli = Cli::try_parse_from(["prowl"]).unwrap();
        let repo = Repo::parse("o/r").unwrap();
        let (client, _) = github::fake::client(tagged_watch());
        let mut branch = Some("main".to_string());
        let baseline = fetch(&cli, &client, &repo, "me", &mut branch, true, true);
        assert!(baseline.issues.is_empty());
        let previous = baseline.sections;
        let shipped_before = shipped(&previous);

        let (client, fake) = github::fake::client(tagged_watch());
        fake.set_response(
            RELEASES,
            500,
            &[],
            r#"{"message":"injected release failure"}"#,
        );
        let current =
            fetch(&cli, &client, &repo, "me", &mut branch, true, true).reconcile(Some(previous));

        assert_eq!(current.issues.len(), 2);
        assert!(current.issues.iter().all(|issue| issue.stale));
        assert!(
            current
                .sections
                .commits
                .as_ref()
                .is_some_and(|stats| stats.available),
            "successful shipment data must be retained"
        );
        assert_eq!(shipped(&current.sections), shipped_before);
        assert_eq!(current.status().lines().count(), 2);
    }

    /// Four watch refreshes through the real schedule, fetch, and merge. The
    /// GraphQL fixture changes on every refresh, so a skipped view must keep the
    /// rows of the refresh that last fetched it.
    #[test]
    fn inactive_view_refreshes_every_second_interval() {
        fn counts(requests: &[String]) -> (usize, usize) {
            let gets = requests.iter().filter(|r| r.starts_with("GET ")).count();
            (gets, requests.len() - gets)
        }
        let cli = Cli::try_parse_from(["prowl"]).unwrap();
        let repo = Repo::parse("o/r").unwrap();
        let start = timefmt::parse_ts("2026-01-10T12:00:00Z").unwrap();
        let fetch_views = |client: &Client, mine, reviews| {
            let mut branch = Some("main".to_string());
            fetch(&cli, client, &repo, "me", &mut branch, mine, reviews)
        };
        let render = |sections: &Sections, view| {
            render_to_string(
                sections,
                &ui(view),
                &Changes::default(),
                None,
                true,
                Profile::Disabled,
            )
        };
        for active in [View::Mine, View::Reviews] {
            let inactive = active.toggle();
            // (GET, POST) of a refresh that skips the inactive view. With Mine
            // active, Mine merged leaves the Reviews request as its own search.
            let skipped_counts = match active {
                View::Mine => (4, 3),
                View::Reviews => (0, 1),
            };
            let (client, fake) = github::fake::client(tagged_watch());
            let mut schedule = Schedule::default();
            let mut last_good = None;
            let mut issues = Vec::new();
            let mut inactive_shown = String::new();
            // (refresh, Ctrl-R, PRs in the fixture, inactive view due)
            for (tick, manual, numbers, inactive_due) in [
                (1, false, &[3, 2, 1][..], true),
                (2, false, &[2, 1][..], false),
                (3, false, &[1][..], true),
                (4, true, &[3][..], true),
            ] {
                let label = format!("{active:?} active, refresh {tick}");
                let routes = tagged_watch_with_merged(numbers);
                let graphql = &routes.iter().find(|(route, _)| route == GRAPHQL).unwrap().1;
                fake.set_response(GRAPHQL, 200, &[], graphql);
                // Baseline: a refresh that fetches both views.
                let (base_client, base_fake) = github::fake::client(routes.clone());
                let base = fetch_views(&base_client, true, true);
                let before = counts(&base_fake.requests());

                let sent = fake.requests().len();
                let due = schedule.next(active, manual);
                let outcome = fetch_views(&client, due.mine, due.reviews);
                let after = counts(&fake.requests()[sent..]);
                let now = start + chrono::TimeDelta::minutes(5 * tick);
                schedule.record(due, &outcome.issues, now);
                let outcome = outcome.merge(last_good.take(), std::mem::take(&mut issues), due);
                println!(
                    "TICK_COUNTS {label}: before GET={} POST={} after GET={} POST={}",
                    before.0, before.1, after.0, after.1
                );

                assert!(due.includes(active), "{label}: {due:?}");
                assert_eq!(due.includes(inactive), inactive_due, "{label}: {due:?}");
                assert_eq!(before, (4, 3), "{label}");
                assert_eq!(
                    after,
                    if inactive_due { before } else { skipped_counts },
                    "{label}"
                );
                assert!(outcome.issues.is_empty(), "{label}: {:?}", outcome.issues);
                assert_eq!(
                    render(&outcome.sections, active),
                    render(&base.sections, active),
                    "{label}: active view output"
                );

                // Tab paints the inactive view from retained data, with no request.
                let sent = fake.requests().len();
                let shown = render(&outcome.sections, inactive);
                assert_eq!(fake.requests().len(), sent, "{label}: Tab requested");
                if inactive_due {
                    assert_eq!(shown, render(&base.sections, inactive), "{label}");
                    assert_eq!(schedule.age_note(inactive, now), None, "{label}");
                } else {
                    assert_eq!(shown, inactive_shown, "{label}: retained rows");
                    assert!(shown.contains("#3"), "{label}: {shown}");
                    assert_eq!(
                        schedule.age_note(inactive, now).as_deref(),
                        Some("updated 5m ago"),
                        "{label}"
                    );
                }
                assert_eq!(schedule.age_note(active, now), None, "{label}");
                inactive_shown = shown;
                last_good = Some(outcome.sections);
                issues = outcome.issues;
            }
            // After Tab, the next refresh fetches the newly active view.
            assert!(schedule.next(inactive, false).includes(inactive));
        }
    }

    #[test]
    fn skipped_view_keeps_its_errors_in_fetch_order() {
        let error = anyhow::anyhow!("boom");
        let mut queue = FetchIssue::new(FetchScope::Queue, &error);
        queue.stale = true;
        let previous = vec![queue, FetchIssue::new(FetchScope::ReviewedMerged, &error)];
        let outcome = FetchOutcome {
            sections: Sections::EMPTY,
            issues: vec![FetchIssue::new(FetchScope::Reviews, &error)],
        }
        .merge(
            None,
            previous,
            Due {
                mine: false,
                reviews: true,
            },
        );

        assert_eq!(
            outcome.status(),
            "error: Merge Queue stale: boom\nerror: Reviews unavailable: boom"
        );
    }

    #[test]
    fn failed_view_keeps_its_age_and_is_retried() {
        let start = timefmt::parse_ts("2026-01-10T12:00:00Z").unwrap();
        let at = |tick: i64| start + chrono::TimeDelta::minutes(5 * tick);
        let failed = [FetchIssue::new(
            FetchScope::ReviewedMerged,
            &anyhow::anyhow!("boom"),
        )];
        let mut schedule = Schedule::default();
        for tick in 1..=2 {
            let due = schedule.next(View::Mine, false);
            schedule.record(due, &[], at(tick));
        }

        let due = schedule.next(View::Mine, false);
        assert!(due.reviews, "{due:?}");
        schedule.record(due, &failed, at(3));

        assert_eq!(
            schedule.age_note(View::Reviews, at(3)).as_deref(),
            Some("updated 10m ago")
        );
        let due = schedule.next(View::Mine, false);
        assert!(due.reviews, "retry after the failure: {due:?}");
    }

    #[test]
    fn partial_view_counts_as_fetched() {
        let start = timefmt::parse_ts("2026-01-10T12:00:00Z").unwrap();
        let at = |tick: i64| start + chrono::TimeDelta::minutes(5 * tick);
        let partial = [FetchIssue {
            warning: true,
            ..FetchIssue::new(FetchScope::ReviewedMerged, &anyhow::anyhow!("denied"))
        }];
        let mut schedule = Schedule::default();
        let due = schedule.next(View::Mine, false);
        assert!(due.reviews, "{due:?}");
        schedule.record(due, &partial, at(1));

        assert_eq!(schedule.age_note(View::Reviews, at(1)), None);
        let due = schedule.next(View::Mine, false);
        assert!(!due.reviews, "partial data is not retried: {due:?}");
    }

    #[test]
    fn one_shot_renders_each_section_error() {
        let cli = Cli::try_parse_from(["prowl", "--only", "mine"]).unwrap();
        let status = "error: My PRs unavailable: first\nerror: Merge Queue stale: second";
        let output = render_to_string_with_status(
            &Sections::EMPTY,
            &Ui::once(&cli),
            &Changes::default(),
            status,
            None,
            true,
            Profile::Disabled,
        );

        assert!(output.contains("error: My PRs unavailable: first"));
        assert!(output.contains("error: Merge Queue stale: second"));
    }

    fn required_rules_graphql(mine_ref: &str, required: &[&str]) -> (String, String) {
        let (_, body) = graphql(&[1]);
        let mut response: serde_json::Value = serde_json::from_str(&body).unwrap();
        let rollup = serde_json::json!({ "contexts": {
            "pageInfo": { "hasNextPage": false, "endCursor": null },
            "checkRunCountsByState": [
                { "state": "SUCCESS", "count": 1 },
                { "state": "FAILURE", "count": 1 }
            ],
            "statusContextCountsByState": [],
            "nodes": [
                {
                    "__typename": "CheckRun", "required": true, "name": "test",
                    "status": "COMPLETED", "conclusion": "SUCCESS",
                    "startedAt": "2026-08-28T10:00:00Z"
                },
                {
                    "__typename": "CheckRun", "required": false, "name": "optional",
                    "status": "COMPLETED", "conclusion": "FAILURE",
                    "startedAt": "2026-08-28T09:00:00Z"
                }
            ]
        } });
        let pr = &mut response["data"]["search"]["nodes"][0];
        pr["baseRef"] = serde_json::json!({ "id": mine_ref });
        pr["commits"] = serde_json::json!({ "nodes": [{ "commit": {
            "id": "PR_HEAD", "statusCheckRollup": rollup
        } }] });
        response["data"]["repository"]["mergeQueue"] = serde_json::json!({
            "entries": { "nodes": [{
                "position": 1,
                "headCommit": { "id": "QUEUE_HEAD", "statusCheckRollup": rollup },
                "pullRequest": {
                    "number": 2, "title": "queued", "url": "https://github.com/o/r/pull/2",
                    "baseRef": { "id": "ref-1" }
                }
            }] }
        });
        response["data"]["repository"]["defaultBranchRef"] = serde_json::json!({
            "id": "ref-1",
            "refUpdateRule": { "requiredStatusCheckContexts": required },
            "rules": {
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": []
            }
        });
        response["data"]["r0"] = serde_json::json!({
            "refUpdateRule": { "requiredStatusCheckContexts": required },
            "rules": {
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": []
            }
        });
        response["data"]["c0"] = serde_json::json!({ "statusCheckRollup": rollup });
        (GRAPHQL.to_string(), response.to_string())
    }

    #[test]
    fn required_rules_follow_the_production_refresh_lifetime() {
        let repo = Repo::parse("o/r").unwrap();
        for (case, mine_ref, per_refresh) in [("shared", "ref-1", 4), ("disjoint", "ref-2", 5)] {
            for required_only in [false, true] {
                let mut cli = Cli::try_parse_from(["prowl", "--only", "mine,queue"]).unwrap();
                cli.required = required_only;
                let mut branch = None;
                let (client, fake) = github::fake::client([]);
                for (index, required) in [&["test", "lint"][..], &["test", "lint", "security"][..]]
                    .into_iter()
                    .enumerate()
                {
                    let refresh = index + 1;
                    let (_, body) = required_rules_graphql(mine_ref, required);
                    fake.set_response(GRAPHQL, 200, &[], &body);
                    let missing = (required.len() - 1) as u64;
                    let expected = status::Checks {
                        fail: u64::from(!required_only),
                        running: missing,
                        pass: 1,
                    };
                    let outcome = fetch(&cli, &client, &repo, "me", &mut branch, true, false);
                    assert!(
                        outcome.issues.is_empty(),
                        "{case}, {required_only}: {:?}",
                        outcome.issues
                    );
                    let sections = outcome.sections;
                    let prs = sections.prs.unwrap();
                    let queue = sections.queue.unwrap();
                    assert_eq!(prs.len(), 1);
                    assert_eq!(queue.len(), 1);
                    assert_eq!(prs[0].checks, expected, "{case}, {required_only}");
                    assert_eq!(queue[0].checks, expected, "{case}, {required_only}");
                    assert_eq!(queue[0].missing_required_checks, missing);
                    assert_eq!(
                        queue[0].build_started_at.as_deref(),
                        Some(if required_only {
                            "2026-08-28T10:00:00Z"
                        } else {
                            "2026-08-28T09:00:00Z"
                        })
                    );
                    let requests = fake.requests();
                    println!(
                        "RULE_REFRESH {case} required={required_only} expected_rules={} refresh={refresh} POST={} checks={:?}",
                        required.len(),
                        requests.len(),
                        prs[0].checks
                    );
                    assert_eq!(requests, vec![GRAPHQL; per_refresh * refresh], "{case}");
                    assert!(branch.is_none());
                }
            }
        }
    }

    fn assert_refresh_matches(
        sections: &Sections,
        refreshed: FetchOutcome,
        before: &[String],
        after: &[String],
        branch_gets: usize,
        name: &str,
    ) {
        assert!(
            refreshed.issues.is_empty(),
            "{name}: {:?}",
            refreshed.issues
        );
        assert_eq!(
            serde_json::to_value(sections).unwrap(),
            serde_json::to_value(&refreshed.sections).unwrap(),
            "{name}"
        );
        assert_eq!(
            &after[before.len()..],
            &before[1 + branch_gets..],
            "{name}: refresh must reuse the resolved branch"
        );
    }

    #[test]
    fn required_rules_are_seeded_by_the_first_mine_request() {
        let cli = Cli::try_parse_from(["prowl", "--only", "mine"]).unwrap();
        let repo = Repo::parse("o/r").unwrap();
        let (_, body) = required_rules_graphql("ref-1", &["test", "lint"]);
        let (client, fake) = github::fake::client([]);
        fake.set_response(GRAPHQL, 200, &[], &body);

        let outcome = fetch(&cli, &client, &repo, "me", &mut None, true, false);

        assert!(outcome.issues.is_empty(), "{:?}", outcome.issues);
        assert_eq!(outcome.sections.prs.unwrap()[0].checks.running, 1);
        assert_eq!(
            fake.requests(),
            [GRAPHQL, GRAPHQL],
            "Mine's inline default rules plus one commit-context request"
        );
    }

    #[test]
    fn unprotected_default_branch_needs_no_rules_request() {
        let cli = Cli::try_parse_from(["prowl", "--only", "mine"]).unwrap();
        let repo = Repo::parse("o/r").unwrap();
        let (_, body) = required_rules_graphql("ref-1", &[]);
        let mut response: serde_json::Value = serde_json::from_str(&body).unwrap();
        response["data"]["repository"]["defaultBranchRef"]["refUpdateRule"] =
            serde_json::Value::Null;
        let (client, fake) = github::fake::client([]);
        fake.set_response(GRAPHQL, 200, &[], &response.to_string());

        let outcome = fetch(&cli, &client, &repo, "me", &mut None, true, false);

        assert!(outcome.issues.is_empty(), "{:?}", outcome.issues);
        assert_eq!(fake.requests(), [GRAPHQL]);
    }

    #[test]
    fn empty_sections_do_not_paginate_default_branch_rules() {
        let repo = Repo::parse("o/r").unwrap();
        for args in [["prowl", "--only", "mine"], ["prowl", "--only", "queue"]] {
            let cli = Cli::try_parse_from(args).unwrap();
            let (_, body) = required_rules_graphql("ref-1", &["test"]);
            let mut response: serde_json::Value = serde_json::from_str(&body).unwrap();
            response["data"]["search"]["nodes"] = serde_json::json!([]);
            response["data"]["repository"]["mergeQueue"]["entries"]["nodes"] =
                serde_json::json!([]);
            response["data"]["repository"]["defaultBranchRef"]["rules"]["pageInfo"] =
                serde_json::json!({ "hasNextPage": true, "endCursor": "NEXT" });
            let (client, fake) = github::fake::client([]);
            fake.set_response(GRAPHQL, 200, &[], &response.to_string());

            let outcome = fetch(&cli, &client, &repo, "me", &mut None, true, false);

            assert!(outcome.issues.is_empty(), "{:?}", outcome.issues);
            assert_eq!(fake.requests(), [GRAPHQL], "{args:?}");
        }
    }

    #[test]
    fn default_branch_startup_requests() {
        let cases: &[(&str, &[&str], bool, usize, usize)] = &[
            ("once_reviews", &["--view", "reviews"], false, 1, 1),
            ("once_queue", &["--only", "queue"], false, 1, 1),
            ("once_mine", &["--only", "mine"], false, 1, 1),
            ("once_merged", &["--only", "merged"], false, 4, 1),
            ("once_shipments", &["--only", "shipments"], false, 6, 0),
            ("once_mine_view", &[], false, 6, 3),
            ("watch_mine_view", &[], true, 6, 3),
            ("watch_reviews", &["--view", "reviews"], true, 6, 3),
            ("watch_mine_only", &["--only", "mine"], true, 1, 2),
        ];
        let repo = Repo::parse("o/r").unwrap();
        for &(name, args, watch, expected_gets, posts) in cases {
            let cli =
                Cli::try_parse_from(std::iter::once("prowl").chain(args.iter().copied())).unwrap();
            let mut routes = if watch && cli.show_merged() {
                tagged_watch()
            } else {
                tagged()
            };
            routes.push(("GET /user".to_string(), r#"{"login":"me"}"#.to_string()));
            routes.push((
                "GET /repos/o/r".to_string(),
                r#"{"default_branch":"trunk"}"#.to_string(),
            ));
            for (route, _) in &mut routes {
                *route = route.replace("...main", "...trunk");
            }
            let (client, fake) = github::fake::client(routes);
            let me = client.me().unwrap();
            let mut branch = None;
            let want_mine = watch || cli.view == View::Mine;
            let want_reviews = watch || cli.view == View::Reviews;
            let outcome = fetch(
                &cli,
                &client,
                &repo,
                &me,
                &mut branch,
                want_mine,
                want_reviews,
            );
            assert!(outcome.issues.is_empty(), "{name}: {:?}", outcome.issues);
            let sections = outcome.sections;
            let requests = fake.requests();
            let gets = requests.iter().filter(|r| r.starts_with("GET ")).count();
            let graphql_count = requests.iter().filter(|r| r.as_str() == GRAPHQL).count();
            println!("REQUEST_COUNTS {name}: GET={gets} POST={graphql_count}");
            println!(
                "SECTIONS {name}: {}",
                serde_json::to_string(&sections).unwrap()
            );
            assert_eq!(gets, expected_gets, "{name}: {requests:?}");
            assert_eq!(graphql_count, posts, "{name}: {requests:?}");
            let branch_gets = usize::from(sections.commits.is_some());
            assert_eq!(
                requests
                    .iter()
                    .filter(|r| r.as_str() == "GET /repos/o/r")
                    .count(),
                branch_gets,
                "{name}: {requests:?}"
            );
            if let Some(stats) = &sections.commits {
                assert!(stats.available, "{name}: {requests:?}");
                assert_eq!(
                    stats.upcoming,
                    Some(commits::Bucket {
                        count: commits::Count {
                            mine: 1,
                            capped: false
                        },
                        url: "https://github.com/o/r/compare/v2...trunk".to_string(),
                    }),
                    "{name}"
                );
            }
            let refreshed = fetch(
                &cli,
                &client,
                &repo,
                &me,
                &mut branch,
                want_mine,
                want_reviews,
            );
            let after = fake.requests();
            assert_refresh_matches(&sections, refreshed, &requests, &after, branch_gets, name);
        }
    }

    #[test]
    fn watch_without_reviews_keeps_mine_merged_as_a_single_search() {
        let cli = Cli::try_parse_from(["prowl"]).unwrap();
        let (client, fake) = github::fake::client(tagged_watch());
        let outcome = fetch(
            &cli,
            &client,
            &Repo::parse("o/r").unwrap(),
            "me",
            &mut Some("main".to_string()),
            true,
            false,
        );

        assert!(outcome.issues.is_empty(), "{:?}", outcome.issues);
        assert!(outcome.sections.merged.is_some());
        assert!(outcome.sections.reviews.is_none());
        assert!(outcome.sections.reviewed_merged.is_none());
        assert_eq!(
            fake.requests()
                .iter()
                .filter(|request| request.as_str() == GRAPHQL)
                .count(),
            3
        );
    }

    #[test]
    fn one_shot_reviews_request_failure_reports_both_sections() {
        let cli = Cli::try_parse_from(["prowl", "--view", "reviews"]).unwrap();
        let (client, fake) = github::fake::client(tagged());
        fake.set_response_on_call(
            GRAPHQL,
            1,
            500,
            &[],
            r#"{"message":"injected request failure"}"#,
        );
        let outcome = fetch(
            &cli,
            &client,
            &Repo::parse("o/r").unwrap(),
            "me",
            &mut Some("main".to_string()),
            false,
            true,
        );

        assert_eq!(
            outcome
                .issues
                .iter()
                .map(|issue| issue.scope)
                .collect::<Vec<_>>(),
            [FetchScope::Reviews, FetchScope::ReviewedMerged]
        );
        assert!(outcome.sections.reviews.is_none());
        assert!(outcome.sections.reviewed_merged.is_none());
        assert_eq!(
            fake.requests()
                .iter()
                .filter(|request| request.as_str() == GRAPHQL)
                .count(),
            1
        );
    }

    #[test]
    fn watch_merged_results_match_the_one_shot_views() {
        let repo = Repo::parse("o/r").unwrap();
        let mut branch = Some("main".to_string());
        let cli = Cli::try_parse_from(["prowl"]).unwrap();
        let (client, fake) = github::fake::client(tagged_watch());
        let watch = fetch(&cli, &client, &repo, "me", &mut branch, true, true);
        assert!(watch.issues.is_empty());
        let watch_posts = fake
            .requests()
            .iter()
            .filter(|request| request.as_str() == GRAPHQL)
            .count();

        let mine_cli = Cli::try_parse_from(["prowl", "--only", "merged"]).unwrap();
        let (client, mine_fake) = github::fake::client(tagged());
        let mine = fetch(
            &mine_cli,
            &client,
            &repo,
            "me",
            &mut Some("main".to_string()),
            true,
            false,
        );
        assert!(mine.issues.is_empty());
        let mine_posts = mine_fake
            .requests()
            .iter()
            .filter(|request| request.as_str() == GRAPHQL)
            .count();

        let reviews_cli = Cli::try_parse_from(["prowl", "--view", "reviews"]).unwrap();
        let (client, reviews_fake) = github::fake::client(tagged());
        let reviews = fetch(
            &reviews_cli,
            &client,
            &repo,
            "me",
            &mut Some("main".to_string()),
            false,
            true,
        );
        assert!(reviews.issues.is_empty());
        let reviews_posts = reviews_fake
            .requests()
            .iter()
            .filter(|request| request.as_str() == GRAPHQL)
            .count();

        assert_eq!(
            serde_json::to_value(watch.sections.merged).unwrap(),
            serde_json::to_value(mine.sections.merged).unwrap()
        );
        assert_eq!(
            serde_json::to_value(watch.sections.reviewed_merged).unwrap(),
            serde_json::to_value(reviews.sections.reviewed_merged).unwrap()
        );
        assert_eq!(
            serde_json::to_value(watch.sections.reviews).unwrap(),
            serde_json::to_value(reviews.sections.reviews).unwrap()
        );
        assert_eq!((watch_posts, mine_posts, reviews_posts), (3, 1, 1));
    }

    #[test]
    fn watch_alias_errors_only_fail_their_sections() {
        for (alias, scope) in [
            ("mineMerged", FetchScope::Merged),
            ("requested", FetchScope::Reviews),
            ("reviewedMerged", FetchScope::ReviewedMerged),
        ] {
            let mut routes = tagged_watch();
            let (_, body) = routes
                .iter_mut()
                .find(|(route, _)| route == GRAPHQL)
                .unwrap();
            let mut response: serde_json::Value = serde_json::from_str(body).unwrap();
            response["data"][alias] = serde_json::Value::Null;
            response["errors"] = serde_json::json!([{
                "message": "Search unavailable",
                "path": [alias]
            }]);
            let partial = response.to_string();

            let (client, fake) = github::fake::client(routes);
            fake.set_response_on_call(GRAPHQL, 1, 200, &[], &partial);
            let outcome = fetch(
                &Cli::try_parse_from(["prowl"]).unwrap(),
                &client,
                &Repo::parse("o/r").unwrap(),
                "me",
                &mut Some("main".to_string()),
                true,
                true,
            );

            assert_eq!(outcome.issues.len(), 1, "{:?}", outcome.issues);
            assert_eq!(outcome.issues[0].scope, scope);
            assert_eq!(
                outcome.issues[0].message,
                "GraphQL error: Search unavailable"
            );
            assert_eq!(
                outcome.sections.merged.is_none(),
                scope == FetchScope::Merged
            );
            assert_eq!(
                outcome.sections.reviews.is_none(),
                scope == FetchScope::Reviews
            );
            assert_eq!(
                outcome.sections.reviewed_merged.is_none(),
                scope == FetchScope::ReviewedMerged
            );
            assert_eq!(
                fake.requests()
                    .iter()
                    .filter(|request| request.as_str() == GRAPHQL)
                    .count(),
                3
            );
        }
    }

    #[test]
    fn default_branch_shipment_sources() {
        let cases = [
            (
                "tagged_main",
                tagged(),
                Some("main"),
                6,
                1,
                "compare/v2...main",
            ),
            (
                "untagged_main",
                untagged(),
                Some("main"),
                5,
                101,
                "commits/main",
            ),
            (
                "untagged_trunk",
                untagged(),
                Some("trunk"),
                5,
                101,
                "commits/trunk",
            ),
        ];
        let cli = Cli::try_parse_from(["prowl", "--only", "shipments"]).unwrap();
        let repo = Repo::parse("o/r").unwrap();
        for (name, mut routes, resolved, gets, mine, path) in cases {
            routes.push(("GET /user".to_string(), r#"{"login":"me"}"#.to_string()));
            if let Some(branch) = resolved {
                routes.push((
                    "GET /repos/o/r".to_string(),
                    serde_json::json!({ "default_branch": branch }).to_string(),
                ));
                for (route, _) in &mut routes {
                    *route = route.replace("sha=main", &format!("sha={branch}"));
                }
            }
            let (client, fake) = github::fake::client(routes);
            let me = client.me().unwrap();
            let mut branch = None;
            let outcome = fetch(&cli, &client, &repo, &me, &mut branch, true, false);
            assert!(outcome.issues.is_empty(), "{name}: {:?}", outcome.issues);
            let sections = outcome.sections;
            let requests = fake.requests();
            println!("REQUEST_COUNTS {name}: GET={} POST=0", requests.len());
            println!(
                "SECTIONS {name}: {}",
                serde_json::to_string(&sections).unwrap()
            );
            assert_eq!(requests.len(), gets, "{name}: {requests:?}");
            assert!(
                requests.iter().all(|r| r.starts_with("GET ")),
                "{name}: {requests:?}"
            );
            let stats = sections.commits.as_ref().unwrap();
            assert!(stats.available, "{name}: {requests:?}");
            assert_eq!(
                stats.upcoming,
                Some(commits::Bucket {
                    count: commits::Count {
                        mine,
                        capped: false
                    },
                    url: format!("https://github.com/o/r/{path}"),
                }),
                "{name}"
            );
            let refreshed = fetch(&cli, &client, &repo, &me, &mut branch, true, false);
            assert!(
                refreshed.issues.is_empty(),
                "{name}: {:?}",
                refreshed.issues
            );
            assert_eq!(
                serde_json::to_value(&sections).unwrap(),
                serde_json::to_value(&refreshed.sections).unwrap(),
                "{name}"
            );
            assert_eq!(&fake.requests()[requests.len()..], &requests[2..], "{name}");
        }
    }

    #[test]
    fn failed_default_branch_lookup_retries_without_guessing() {
        let cli = Cli::try_parse_from(["prowl"]).unwrap();
        let repo = Repo::parse("o/r").unwrap();
        let mut routes = tagged();
        routes.push((
            "GET /repos/o/r".to_string(),
            r#"{"default_branch":"trunk"}"#.to_string(),
        ));
        routes.push((
            "GET /repos/o/r/compare/v2...trunk".to_string(),
            serde_json::json!({
                "total_commits": 2,
                "commits": [
                    commit("me", "feat: four (#4)"),
                    commit("me", "feat: five (#5)"),
                ]
            })
            .to_string(),
        ));
        let (client, fake) = github::fake::client(routes);
        fake.set_response_on_call(
            "GET /repos/o/r",
            1,
            500,
            &[],
            r#"{"message":"temporary failure"}"#,
        );
        let mut branch = None;
        let first = fetch(&cli, &client, &repo, "me", &mut branch, true, false);
        let first_requests = fake.requests();
        let counts = |requests: &[String]| {
            (
                requests
                    .iter()
                    .filter(|request| request.starts_with("GET "))
                    .count(),
                requests
                    .iter()
                    .filter(|request| request.as_str() == GRAPHQL)
                    .count(),
                requests
                    .iter()
                    .filter(|request| request.as_str() == "GET /repos/o/r")
                    .count(),
            )
        };

        assert_eq!(branch, None);
        assert_eq!(first.issues.len(), 1);
        assert_eq!(first.issues[0].scope, FetchScope::Upcoming);
        assert!(first.status().contains("upcoming unavailable"));
        let first_stats = first.sections.commits.as_ref().unwrap();
        assert!(first_stats.available);
        assert_eq!(first_stats.upcoming, None);
        assert_eq!(
            first_stats
                .releases
                .iter()
                .map(|release| { (release.tag.as_str(), release.count.as_ref().unwrap().mine) })
                .collect::<Vec<_>>(),
            [("v2", 1), ("v1", 1)]
        );
        assert_eq!(
            shipped(&first.sections),
            [(3, None), (2, Some("v2".into())), (1, Some("v1".into()))]
        );
        assert!(
            first_requests
                .iter()
                .all(|request| !request.contains("...main") && !request.contains("sha=main")),
            "{first_requests:?}"
        );
        assert_eq!(counts(&first_requests), (4, 3, 1));
        let rendered = body(&first.sections, &Ui::once(&cli));
        assert!(rendered.contains("upcoming"));
        assert!(rendered.contains("unavailable"));

        let recovered = fetch(&cli, &client, &repo, "me", &mut branch, true, false);
        let all_requests = fake.requests();
        let refresh_requests = &all_requests[first_requests.len()..];
        assert!(recovered.issues.is_empty(), "{:?}", recovered.issues);
        assert_eq!(branch.as_deref(), Some("trunk"));
        let recovered_stats = recovered.sections.commits.as_ref().unwrap();
        let upcoming = recovered_stats.upcoming.as_ref().unwrap();
        assert_eq!(upcoming.count.mine, 2);
        assert_eq!(upcoming.url, "https://github.com/o/r/compare/v2...trunk");
        assert_eq!(
            shipped(&recovered.sections),
            [(3, None), (2, Some("v2".into())), (1, Some("v1".into()))]
        );
        assert!(
            refresh_requests
                .iter()
                .all(|request| !request.contains("...main") && !request.contains("sha=main")),
            "{refresh_requests:?}"
        );
        assert_eq!(counts(refresh_requests), (5, 3, 1));
        assert_eq!(counts(&all_requests), (9, 6, 2));
    }

    fn shipped(sections: &Sections) -> Vec<(i64, Option<String>)> {
        sections
            .merged
            .iter()
            .flatten()
            .map(|row| (row.number, row.release.as_ref().map(|r| r.tag.clone())))
            .collect()
    }

    #[test]
    fn hidden_shipments_fetch_requests() {
        let (sections, requests) = fetch_offline(&["prowl", "--only", "merged"], tagged());
        assert_eq!(
            requests,
            [
                GRAPHQL,
                RELEASES,
                "GET /repos/o/r/compare/v1...v2",
                "GET /repos/o/r/commits?sha=v1&author=me&per_page=100&page=1",
            ]
        );
        assert_eq!(
            shipped(&sections),
            [
                (3, None),
                (2, Some("v2".to_string())),
                (1, Some("v1".to_string())),
            ]
        );
        assert!(sections.commits.is_none());

        let (sections, requests) = fetch_offline(&["prowl", "--only", "merged"], untagged());
        assert_eq!(requests, [GRAPHQL, RELEASES]);
        assert_eq!(shipped(&sections), [(1, None)]);
        assert!(sections.commits.is_none());
    }

    #[test]
    fn hidden_shipments_scan_only_until_displayed_prs_are_mapped() {
        let (sections, requests) =
            fetch_offline(&["prowl", "--only", "merged"], tagged_with_merged(&[]));
        assert_eq!(requests, [GRAPHQL]);
        assert_eq!(shipped(&sections), Vec::<(i64, Option<String>)>::new());

        let (sections, requests) = fetch_offline(
            &["prowl", "--only", "merged", "--merged-limit", "0"],
            tagged_with_merged(&[2]),
        );
        assert_eq!(requests, [GRAPHQL]);
        assert_eq!(shipped(&sections), Vec::<(i64, Option<String>)>::new());

        let (sections, requests) =
            fetch_offline(&["prowl", "--only", "merged"], tagged_with_merged(&[2]));
        assert_eq!(
            requests,
            [GRAPHQL, RELEASES, "GET /repos/o/r/compare/v1...v2",]
        );
        assert_eq!(shipped(&sections), [(2, Some("v2".to_string()))]);

        let (sections, requests) =
            fetch_offline(&["prowl", "--only", "merged"], tagged_with_merged(&[3]));
        assert_eq!(
            requests,
            [
                GRAPHQL,
                RELEASES,
                "GET /repos/o/r/compare/v1...v2",
                "GET /repos/o/r/commits?sha=v1&author=me&per_page=100&page=1",
            ]
        );
        assert_eq!(shipped(&sections), [(3, None)]);
    }

    #[test]
    fn hidden_shipments_watch_reuses_the_batched_merged_rows() {
        let args = ["prowl", "--only", "merged"];
        let cli = Cli::try_parse_from(args).unwrap();
        let repo = Repo::parse("o/r").unwrap();
        for numbers in [&[][..], &[2][..], &[3][..]] {
            let (once, requests) = fetch_offline(&args, tagged_with_merged(numbers));
            let (client, fake) = github::fake::client(tagged_watch_with_merged(numbers));
            let watch = fetch(
                &cli,
                &client,
                &repo,
                "me",
                &mut Some("main".to_string()),
                true,
                true,
            );

            assert!(watch.issues.is_empty(), "{:?}", watch.issues);
            assert_eq!(fake.requests(), requests);
            assert_eq!(
                serde_json::to_value(&watch.sections.merged).unwrap(),
                serde_json::to_value(&once.merged).unwrap()
            );
            assert_eq!(watch.sections.reviewed_merged.unwrap().len(), numbers.len());
        }
    }

    #[test]
    fn shown_shipments_fetch_requests() {
        let (sections, requests) = fetch_offline(&["prowl"], tagged());
        assert_eq!(
            requests,
            [
                RELEASES,
                "GET /repos/o/r/compare/v2...main",
                "GET /repos/o/r/compare/v1...v2",
                "GET /repos/o/r/commits?sha=v1&author=me&per_page=100&page=1",
                GRAPHQL,
                GRAPHQL,
                GRAPHQL,
            ]
        );
        assert_eq!(
            shipped(&sections),
            [
                (3, None),
                (2, Some("v2".to_string())),
                (1, Some("v1".to_string())),
            ]
        );
        let count = |mine| commits::Count {
            mine,
            capped: false,
        };
        let stats = sections.commits.unwrap();
        assert_eq!(
            stats.upcoming,
            Some(commits::Bucket {
                count: count(1),
                url: "https://github.com/o/r/compare/v2...main".to_string(),
            })
        );
        let releases: Vec<_> = stats
            .releases
            .iter()
            .map(|r| (r.tag.as_str(), r.count.unwrap()))
            .collect();
        assert_eq!(releases, [("v2", count(1)), ("v1", count(1))]);

        let (sections, requests) = fetch_offline(&["prowl"], untagged());
        assert_eq!(
            requests,
            [
                RELEASES,
                "GET /repos/o/r/commits?sha=main&author=me&per_page=100&page=1",
                "GET /repos/o/r/commits?sha=main&author=me&per_page=100&page=2",
                GRAPHQL,
                GRAPHQL,
                GRAPHQL,
            ]
        );
        assert_eq!(shipped(&sections), [(1, None)]);
        let stats = sections.commits.unwrap();
        assert_eq!(
            stats.upcoming,
            Some(commits::Bucket {
                count: count(101),
                url: "https://github.com/o/r/commits/main".to_string(),
            })
        );
        assert_eq!(stats.releases, []);
    }
}

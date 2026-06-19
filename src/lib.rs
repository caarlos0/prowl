//! prowl — watch a repo's open PRs, merge queue, and recently merged PRs.
//!
//! The crate is split into a small library (this file plus its modules) and a
//! thin binary so the parsing/rendering/change-detection logic can be exercised
//! by offline, fixture-based tests under `tests/`.

pub mod changes;
pub mod cli;
pub mod commits;
pub mod gh;
pub mod merged;
pub mod model;
pub mod prs;
pub mod queue;
pub mod render;
pub mod status;
pub mod timefmt;

use anyhow::Result;
use changes::{Changes, Tracker};
use clap::Parser;
use cli::Cli;
use gh::Repo;
use std::io::{IsTerminal, Write};

/// A fetched snapshot of every enabled section (`None` = section disabled).
struct Sections {
    merged: Option<Vec<merged::MergedRow>>,
    queue: Option<Vec<queue::QueueRow>>,
    prs: Option<Vec<prs::PrRow>>,
    commits: Option<commits::CommitStats>,
}

fn fetch(cli: &Cli, repo: &Repo, me: &str, default_branch: &str) -> Result<Sections> {
    let merged = if cli.show_merged() {
        let since = timefmt::since_date(&cli.merged_window);
        let nodes = model::fetch_merged(repo, me, &since, cli.merged_limit)?;
        Some(merged::build_rows(nodes, cli.merged_limit))
    } else {
        None
    };
    let queue = if cli.show_queue() {
        Some(queue::build_rows(model::fetch_queue(repo)?, me))
    } else {
        None
    };
    let prs = if cli.show_mine() {
        Some(prs::build_rows(model::fetch_my_prs(repo, me)?))
    } else {
        None
    };
    // Best-effort: a failure here (no releases, empty repo, ...) degrades to an
    // "unavailable" line rather than taking down the whole dashboard.
    let commits = if cli.show_commits() {
        Some(
            commits::fetch(repo, me, default_branch)
                .unwrap_or_else(|_| commits::CommitStats::unavailable()),
        )
    } else {
        None
    };
    Ok(Sections {
        merged,
        queue,
        prs,
        commits,
    })
}

/// The status glyphs and `STATE` values currently on screen, for the legend.
fn legend(s: &Sections) -> (Vec<status::Status>, bool, Vec<String>) {
    let mut statuses: Vec<status::Status> = Vec::new();
    let mut has_none = false;
    let mut states: Vec<String> = Vec::new();
    if let Some(rows) = &s.prs {
        for r in rows {
            match r.status {
                Some(st) if !statuses.contains(&st) => statuses.push(st),
                Some(_) => {}
                None => has_none = true,
            }
            if let Some(ms) = &r.merge_state
                && !states.contains(ms)
            {
                states.push(ms.clone());
            }
        }
    }
    if let Some(rows) = &s.merged
        && !rows.is_empty()
        && !statuses.contains(&status::Status::Merged)
    {
        statuses.push(status::Status::Merged);
    }
    (statuses, has_none, states)
}

/// Render the section bodies (no screen-clear, no status line): Open PRs, then
/// Merge Queue, then Merged PRs, then the reference legend. Empty sections
/// collapse to a dim one-liner. Rows that changed since the previous refresh
/// (per `changes`) are flagged with a leading marker.
fn render_body(
    s: &Sections,
    cli: &Cli,
    repo: &Repo,
    me: &str,
    changes: &Changes,
    styled: bool,
) -> String {
    let mut f = String::new();
    let ascii = cli.ascii || !styled;
    let slug = repo.slug();

    if let Some(rows) = &s.prs {
        if rows.is_empty() {
            let msg = format!("No open PRs by {me} in {slug}.");
            f.push_str(&render::empty_line(&msg, styled));
            f.push('\n');
        } else {
            f.push_str(&render::header(
                "Open PRs",
                status::LAVENDER,
                Some(rows.len()),
                styled,
            ));
            f.push('\n');
            let table = prs::to_table(rows, ascii, &changes.status_changed);
            f.push_str(&render::render_table(&table, styled));
        }
        f.push('\n');
    }

    if let Some(rows) = &s.queue {
        if rows.is_empty() {
            let msg = format!("No merge queue (or it is empty) for {slug}.");
            f.push_str(&render::empty_line(&msg, styled));
            f.push('\n');
        } else {
            f.push_str(&render::header(
                "Merge Queue",
                status::BLUE,
                Some(rows.len()),
                styled,
            ));
            f.push('\n');
            f.push_str(&render::render_table(&queue::to_table(rows), styled));
        }
        f.push('\n');
    }

    if let Some(rows) = &s.merged {
        if rows.is_empty() {
            let msg = format!(
                "No PRs merged by {me} in {slug} in the last {}.",
                cli.merged_window.raw
            );
            f.push_str(&render::empty_line(&msg, styled));
            f.push('\n');
        } else {
            f.push_str(&render::header(
                "Merged PRs",
                status::MAUVE,
                Some(rows.len()),
                styled,
            ));
            f.push('\n');
            let table = merged::to_table(rows, ascii, &changes.newly_merged);
            f.push_str(&render::render_table(&table, styled));
        }
        f.push('\n');
    }

    if let Some(stats) = &s.commits {
        render_commits(&mut f, stats, styled);
        f.push('\n');
    }

    if !cli.no_reference {
        let (statuses, has_none, states) = legend(s);
        if !statuses.is_empty() || has_none || !states.is_empty() {
            f.push_str(&render::reference(
                &statuses, has_none, &states, ascii, styled,
            ));
            f.push('\n');
        }
    }
    f
}

/// The dim trailing status line plus a newline.
fn trailing(change: Option<bool>, next: Option<&str>, styled: bool) -> String {
    let mut s = render::status_line(&timefmt::now_hms(), change, next, styled);
    s.push('\n');
    s
}

/// Render the "Commits" section: my commit counts for the previous and next
/// stable release.
fn render_commits(f: &mut String, stats: &commits::CommitStats, styled: bool) {
    if !stats.available {
        f.push_str(&render::empty_line("Commit stats unavailable.", styled));
        f.push('\n');
        return;
    }
    f.push_str(&render::header("Commits", status::TEAL, None, styled));
    f.push('\n');

    let count = |c: &commits::Count| format!("{}{}", c.mine, if c.capped { "+" } else { "" });

    let prev = match (&stats.previous_tag, &stats.previous) {
        (Some(tag), Some(c)) => format!("  previous {tag}: {} by you", count(c)),
        _ => "  previous: no stable release yet".to_string(),
    };
    f.push_str(&render::empty_line(&prev, styled));
    f.push('\n');

    let next = match &stats.next {
        Some(c) => format!("  next: {} by you", count(c)),
        None => "  next: \u{2014}".to_string(),
    };
    f.push_str(&render::empty_line(&next, styled));
    f.push('\n');
}

/// A dim trailing line reporting a transient error (last good data is kept).
fn error_trailing(msg: &str, next: Option<&str>, styled: bool) -> String {
    let next_part = match next {
        Some(n) => format!(" \u{00b7} next {n}"),
        None => String::new(),
    };
    let line = format!(
        "updated {} \u{2014} error: {msg}{next_part}",
        timefmt::now_hms()
    );
    let mut s = render::empty_line(&line, styled);
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

/// Entry point: parse the CLI, resolve repo + user, then render once or watch.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let styled = std::io::stdout().is_terminal();
    let watch = styled && !cli.once;

    // In watch mode, hide the cursor for the whole session (starting with the
    // loading screen) and make sure it is restored on every exit path: the
    // CursorGuard handles a normal/`?`-error return, and the Ctrl-C handler
    // handles SIGINT (which would otherwise skip the guard).
    //
    // Paint a loading screen up front, too: resolving the repo/user and the
    // first fetch are several `gh` calls, and we don't want to stare at a blank
    // or stale screen until the first frame is ready.
    let _cursor = if watch {
        print!("{}", render::HIDE_CURSOR);
        let _ = ctrlc::set_handler(|| {
            print!("{}", render::SHOW_CURSOR);
            let _ = std::io::stdout().flush();
            std::process::exit(130);
        });
        println!("{}{}", render::clear(), render::loading(styled));
        std::io::stdout().flush()?;
        Some(CursorGuard)
    } else {
        None
    };

    let repo = match &cli.repo {
        Some(slug) => Repo::parse(slug)?,
        None => gh::detect_repo()?,
    };
    let me = gh::me()?;
    // The default branch is the head of the "next release" commit range.
    // Resolved once; falls back to `main` if it can't be determined.
    let default_branch = gh::default_branch(&repo).unwrap_or_else(|_| "main".to_string());

    // Single render: --once, or whenever stdout is not a TTY.
    if cli.once || !styled {
        let sections = fetch(&cli, &repo, &me, &default_branch)?;
        let frame = render_body(&sections, &cli, &repo, &me, &Changes::default(), styled)
            + &trailing(None, None, styled);
        print!("{frame}");
        std::io::stdout().flush()?;
        return Ok(());
    }

    // Watch loop. Each tick clears the screen and re-renders; the bell rings
    // once when a PR of mine merges or an open PR's status changes, and those
    // rows are flagged on the redraw. A failed fetch keeps the last good data,
    // shows a dim error line, and does not ring.
    let mut prev: Option<Tracker> = None;
    let mut last_good: Option<Sections> = None;
    loop {
        match fetch(&cli, &repo, &me, &default_branch) {
            Ok(sections) => {
                let tracker = Tracker::build(sections.prs.as_deref(), sections.merged.as_deref());
                let changes = prev.as_ref().map(|p| tracker.diff(p)).unwrap_or_default();
                let bell = changes.any();
                let change_display = prev.as_ref().map(|_| bell);
                let next = timefmt::next_hms(cli.interval.dur);

                let mut frame = String::from(render::clear());
                frame.push_str(&render_body(&sections, &cli, &repo, &me, &changes, styled));
                frame.push_str(&trailing(change_display, Some(&next), styled));
                print!("{frame}");
                std::io::stdout().flush()?;

                if bell && !cli.no_bell {
                    render::ring_bell();
                }
                prev = Some(tracker);
                last_good = Some(sections);
            }
            Err(e) => {
                let next = timefmt::next_hms(cli.interval.dur);
                let mut frame = String::from(render::clear());
                if let Some(good) = &last_good {
                    frame.push_str(&render_body(
                        good,
                        &cli,
                        &repo,
                        &me,
                        &Changes::default(),
                        styled,
                    ));
                }
                frame.push_str(&error_trailing(&short_error(&e), Some(&next), styled));
                print!("{frame}");
                std::io::stdout().flush()?;
            }
        }
        std::thread::sleep(cli.interval.dur);
    }
}

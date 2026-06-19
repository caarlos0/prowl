//! prowl — watch a repo's recently merged PRs, merge queue, and your open PRs.
//!
//! The crate is split into a small library (this file plus its modules) and a
//! thin binary so the parsing/rendering/change-detection logic can be exercised
//! by offline, fixture-based tests under `tests/`.

pub mod cli;
pub mod gh;
pub mod merged;
pub mod model;
pub mod prs;
pub mod queue;
pub mod render;
pub mod snapshot;
pub mod status;
pub mod timefmt;

use anyhow::Result;
use clap::Parser;
use cli::Cli;
use gh::Repo;
use snapshot::Snapshot;
use std::io::{IsTerminal, Write};

/// A fetched snapshot of every enabled section (`None` = section disabled).
struct Sections {
    merged: Option<Vec<merged::MergedRow>>,
    queue: Option<Vec<queue::QueueRow>>,
    prs: Option<Vec<prs::PrRow>>,
}

impl Sections {
    fn snapshot(&self) -> Snapshot {
        Snapshot::build(
            self.merged.as_deref(),
            self.queue.as_deref(),
            self.prs.as_deref(),
        )
    }
}

fn fetch(cli: &Cli, repo: &Repo, me: &str) -> Result<Sections> {
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
    Ok(Sections { merged, queue, prs })
}

/// Render the section bodies (no screen-clear, no status line): Recently
/// Merged, then Merge Queue, then My PRs. Empty sections collapse to a dim
/// one-liner with no header bar.
fn render_body(s: &Sections, cli: &Cli, repo: &Repo, me: &str, styled: bool) -> String {
    let mut f = String::new();
    let ascii = cli.ascii || !styled;
    let slug = repo.slug();

    if let Some(rows) = &s.merged {
        if rows.is_empty() {
            let msg = format!(
                "No PRs merged by {me} in {slug} in the last {}.",
                cli.merged_window.raw
            );
            f.push_str(&render::empty_line(&msg, styled));
            f.push('\n');
        } else {
            f.push_str(&render::header("Recently Merged", status::MAUVE, rows.len(), styled));
            f.push('\n');
            f.push_str(&render::render_table(&merged::to_table(rows, ascii), styled));
        }
        f.push('\n');
    }

    if let Some(rows) = &s.queue {
        if rows.is_empty() {
            let msg = format!("No merge queue (or it is empty) for {slug}.");
            f.push_str(&render::empty_line(&msg, styled));
            f.push('\n');
        } else {
            f.push_str(&render::header("Merge Queue", status::BLUE, rows.len(), styled));
            f.push('\n');
            f.push_str(&render::render_table(&queue::to_table(rows), styled));
        }
        f.push('\n');
    }

    if let Some(rows) = &s.prs {
        if rows.is_empty() {
            let msg = format!("No open PRs by {me} in {slug}.");
            f.push_str(&render::empty_line(&msg, styled));
            f.push('\n');
        } else {
            f.push_str(&render::header("My PRs", status::LAVENDER, rows.len(), styled));
            f.push('\n');
            f.push_str(&render::render_table(&prs::to_table(rows, ascii), styled));
        }
        f.push('\n');
    }
    f
}

/// The dim trailing status line plus a newline.
fn trailing(change: Option<bool>, styled: bool) -> String {
    let mut s = render::status_line(&timefmt::now_hms(), change, styled);
    s.push('\n');
    s
}

/// A dim trailing line reporting a transient error (last good data is kept).
fn error_trailing(msg: &str, styled: bool) -> String {
    let line = format!("updated {} \u{2014} error: {msg}", timefmt::now_hms());
    let mut s = render::empty_line(&line, styled);
    s.push('\n');
    s
}

/// First line of an error, truncated, for the one-line error status.
fn short_error(e: &anyhow::Error) -> String {
    let full = format!("{e:#}");
    let first = full.lines().next().unwrap_or_default();
    if first.chars().count() > 120 {
        format!("{}…", first.chars().take(119).collect::<String>())
    } else {
        first.to_string()
    }
}

fn maybe_notify(cli: &Cli, repo: &Repo, now: &Snapshot, prev: Option<&Snapshot>) {
    if !cli.notify {
        return;
    }
    let body = match prev.map(|p| now.newly_merged(p)) {
        Some(merged) if !merged.is_empty() => {
            let list = merged
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("merged: {list}")
        }
        _ => "dashboard changed".to_string(),
    };
    notify_send(repo, &body);
}

#[cfg(feature = "notify")]
fn notify_send(repo: &Repo, body: &str) {
    let _ = notify_rust::Notification::new()
        .summary(&format!("prowl: {}", repo.slug()))
        .body(body)
        .show();
}

#[cfg(not(feature = "notify"))]
fn notify_send(_repo: &Repo, _body: &str) {}

/// Entry point: parse the CLI, resolve repo + user, then render once or watch.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let styled = std::io::stdout().is_terminal();

    let repo = match &cli.repo {
        Some(slug) => Repo::parse(slug)?,
        None => gh::detect_repo()?,
    };
    let me = gh::me()?;

    if cli.notify && !cfg!(feature = "notify") {
        eprintln!(
            "prowl: --notify ignored (rebuild with `--features notify` for desktop notifications)"
        );
    }

    // Single render: --once, or whenever stdout is not a TTY.
    if cli.once || !styled {
        let sections = fetch(&cli, &repo, &me)?;
        let frame = render_body(&sections, &cli, &repo, &me, styled) + &trailing(None, styled);
        print!("{frame}");
        std::io::stdout().flush()?;
        return Ok(());
    }

    // Watch loop. Each tick clears the screen and re-renders; the bell rings
    // exactly once per changed refresh. A failed fetch keeps the last good
    // data, shows a dim error line, and does not ring the bell.
    let mut prev: Option<Snapshot> = None;
    let mut last_good: Option<Sections> = None;
    loop {
        match fetch(&cli, &repo, &me) {
            Ok(sections) => {
                let snap = sections.snapshot();
                let changed = snapshot::should_ring(prev.as_ref(), &snap);
                let change_display = prev.as_ref().map(|p| *p != snap);

                let mut frame = String::from(render::clear());
                frame.push_str(&render_body(&sections, &cli, &repo, &me, styled));
                frame.push_str(&trailing(change_display, styled));
                print!("{frame}");
                std::io::stdout().flush()?;

                if changed {
                    if !cli.no_bell {
                        render::ring_bell();
                    }
                    maybe_notify(&cli, &repo, &snap, prev.as_ref());
                }
                prev = Some(snap);
                last_good = Some(sections);
            }
            Err(e) => {
                let mut frame = String::from(render::clear());
                if let Some(good) = &last_good {
                    frame.push_str(&render_body(good, &cli, &repo, &me, styled));
                }
                frame.push_str(&error_trailing(&short_error(&e), styled));
                print!("{frame}");
                std::io::stdout().flush()?;
            }
        }
        std::thread::sleep(cli.interval.dur);
    }
}

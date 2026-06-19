// TODO(quality): remove this once every module is wired into the render path.
#![allow(dead_code)]

mod cli;
mod gh;
mod merged;
mod model;
mod prs;
mod queue;
mod render;
mod status;
mod timefmt;

use anyhow::Result;
use clap::Parser;
use cli::Cli;
use gh::Repo;
use std::io::{IsTerminal, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// A fetched snapshot of every enabled section (`None` = section disabled).
struct Sections {
    merged: Option<Vec<merged::MergedRow>>,
    queue: Option<Vec<queue::QueueRow>>,
    prs: Option<Vec<prs::PrRow>>,
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

/// Render the whole frame: Recently Merged, then Merge Queue, then My PRs, then
/// the status line. Empty sections collapse to a dim one-liner.
fn render_frame(
    s: &Sections,
    cli: &Cli,
    repo: &Repo,
    me: &str,
    change: Option<bool>,
    styled: bool,
    clear: bool,
) -> String {
    let mut f = String::new();
    if clear {
        f.push_str(render::clear());
    }
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

    f.push_str(&render::status_line(&timefmt::now_hms(), change, styled));
    f.push('\n');
    f
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let styled = std::io::stdout().is_terminal();

    let repo = match &cli.repo {
        Some(slug) => Repo::parse(slug)?,
        None => gh::detect_repo()?,
    };
    let me = gh::me()?;

    // Single render for now (--once / piped). The interval watch loop with
    // change detection and the bell is wired in the next commit.
    let sections = fetch(&cli, &repo, &me)?;
    let frame = render_frame(&sections, &cli, &repo, &me, None, styled, false);
    print!("{frame}");
    std::io::stdout().flush()?;
    Ok(())
}

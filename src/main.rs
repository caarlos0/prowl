// TODO(quality): remove this once every module is wired into the render path.
#![allow(dead_code)]

mod cli;
mod gh;
mod model;
mod timefmt;

use anyhow::Result;
use clap::Parser;
use cli::Cli;
use gh::Repo;
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

fn run() -> Result<()> {
    let cli = Cli::parse();

    let repo = match &cli.repo {
        Some(slug) => Repo::parse(slug)?,
        None => gh::detect_repo()?,
    };
    let me = gh::me()?;
    eprintln!("repo={} me={me}", repo.slug());

    if cli.show_merged() {
        let since = timefmt::since_date(&cli.merged_window);
        let merged = model::fetch_merged(&repo, &me, &since, cli.merged_limit)?;
        eprintln!("recently merged (since {since}): {}", merged.len());
    }
    if cli.show_queue() {
        let queue = model::fetch_queue(&repo)?;
        eprintln!("merge queue entries: {}", queue.len());
    }
    if cli.show_mine() {
        let mine = model::fetch_my_prs(&repo, &me)?;
        eprintln!("my open PRs: {}", mine.len());
    }
    Ok(())
}

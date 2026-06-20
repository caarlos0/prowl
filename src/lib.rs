//! prowl — watch a repo's open PRs, merge queue, and recently merged PRs.
//!
//! The crate is split into a small library (this file plus its modules) and a
//! thin binary so the parsing/rendering/change-detection logic can be exercised
//! by offline, fixture-based tests under `tests/`.

pub mod auth;
pub mod cache;
pub mod changes;
pub mod cli;
pub mod commits;
pub mod github;
pub mod merged;
pub mod model;
pub mod prs;
pub mod queue;
pub mod render;
pub mod status;
pub mod term;
pub mod timefmt;

use anyhow::{Context, Result};
use changes::{Changes, Tracker};
use clap::Parser;
use cli::Cli;
use github::{Client, Repo};
use std::io::{IsTerminal, Write};
use unicode_width::UnicodeWidthStr;

/// A fetched snapshot of every enabled section (`None` = section disabled).
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Sections {
    merged: Option<Vec<merged::MergedRow>>,
    queue: Option<Vec<queue::QueueRow>>,
    prs: Option<Vec<prs::PrRow>>,
    commits: Option<commits::CommitStats>,
}

fn fetch(
    cli: &Cli,
    client: &Client,
    repo: &Repo,
    me: &str,
    default_branch: &str,
) -> Result<Sections> {
    let merged = if cli.show_merged() {
        let since = timefmt::since_date(&cli.merged_window);
        let nodes = model::fetch_merged(client, repo, me, &since, cli.merged_limit)?;
        Some(merged::build_rows(nodes, cli.merged_limit))
    } else {
        None
    };
    let queue = if cli.show_queue() {
        Some(queue::build_rows(model::fetch_queue(client, repo)?, me))
    } else {
        None
    };
    let prs = if cli.show_mine() {
        Some(prs::build_rows(model::fetch_my_prs(client, repo, me)?))
    } else {
        None
    };
    // Best-effort: a failure here (no releases, empty repo, ...) degrades to an
    // "unavailable" line rather than taking down the whole dashboard.
    let commits = if cli.show_shipments() {
        Some(
            commits::fetch(client, repo, me, default_branch)
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

/// Synthetic dashboard data for `--demo` (screenshots): no auth, repo, or
/// network. Times are relative to now so the ages look fresh. Temporary.
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

    let qrow = |position, number, author: &str, title: &str, mine| queue::QueueRow {
        position,
        number,
        author: author.to_string(),
        title: title.to_string(),
        url: format!("https://github.com/caarlos0/prowl/pull/{number}"),
        mine,
    };
    let queue = vec![
        qrow(
            1,
            118,
            "caarlos0",
            "feat(queue): inline merge-queue position",
            true,
        ),
        qrow(
            2,
            131,
            "dependabot[bot]",
            "build(deps): bump anstyle to 1.1",
            false,
        ),
        qrow(3, 117, "octocat", "docs: clarify the --only flag", false),
    ];

    let mrow = |number, title: &str, secs| merged::MergedRow {
        number,
        title: title.to_string(),
        url: format!("https://github.com/caarlos0/prowl/pull/{number}"),
        base: "main".to_string(),
        merged_at: Some(ago(secs)),
        updated_at: Some(ago(secs)),
    };
    let merged = vec![
        mrow(119, "feat(status): ignore phantom check suites", 720),
        mrow(116, "fix(github): exact-match the remote host", 7200),
        mrow(112, "ci: build a snapshot on pull requests", 86_400),
        mrow(108, "feat(render): OSC-8 hyperlinks for URLs", 259_200),
    ];

    let count = |mine, capped| commits::Count { mine, capped };
    let commits = commits::CommitStats {
        available: true,
        upcoming: Some(count(7, false)),
        releases: vec![
            commits::ReleaseCount {
                tag: "v0.4.0".to_string(),
                count: count(12, false),
            },
            commits::ReleaseCount {
                tag: "v0.3.0".to_string(),
                count: count(9, false),
            },
            commits::ReleaseCount {
                tag: "v0.2.0".to_string(),
                count: count(31, true),
            },
            commits::ReleaseCount {
                tag: "v0.1.0".to_string(),
                count: count(18, false),
            },
        ],
    };

    Sections {
        merged: Some(merged),
        queue: Some(queue),
        prs: Some(prs),
        commits: Some(commits),
    }
}

/// Render one PR section: a counted header, then either its table or, when
/// empty, a dim placeholder. `table` is `None` exactly when the section is
/// empty (empties are filtered out before alignment).
fn section(
    f: &mut String,
    title: &str,
    accent: status::Rgb,
    count: usize,
    empty_msg: &str,
    table: Option<&render::Table>,
    styled: bool,
) {
    f.push_str(&render::header(
        title,
        accent,
        Some(&count.to_string()),
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

/// Render the section bodies (no screen-clear, no status line): My open PRs,
/// then Merge Queue, then My merged PRs, then My Shipments. Each PR section
/// always shows its header (with a count); an empty section follows it with a
/// dim placeholder one-liner. Rows that changed since the previous refresh (per
/// `changes`) are flagged with a leading marker. The help legend is rendered
/// separately (below the status line) by `help_block`.
fn render_body(s: &Sections, cli: &Cli, changes: &Changes, styled: bool) -> String {
    let mut f = String::new();
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

    if let Some(rows) = &s.prs {
        section(
            &mut f,
            "My open PRs",
            status::LAVENDER,
            rows.len(),
            "No open PRs.",
            prs_table.as_ref(),
            styled,
        );
    }

    if let Some(rows) = &s.queue {
        section(
            &mut f,
            "Merge Queue",
            status::BLUE,
            rows.len(),
            "No merge queue.",
            queue_table.as_ref(),
            styled,
        );
    }

    if let Some(rows) = &s.merged {
        section(
            &mut f,
            "My merged PRs",
            status::MAUVE,
            rows.len(),
            "No recent merged PRs.",
            merged_table.as_ref(),
            styled,
        );
    }

    if let Some(stats) = &s.commits {
        render_commits(&mut f, stats, styled);
        f.push('\n');
    }

    f
}

/// The help legend (the complete status-glyph + `STATE` reference), shown at
/// the very bottom. Empty (no leading/trailing blank) when `show_help` is
/// false; ends with a newline otherwise.
fn help_block(cli: &Cli, show_help: bool, styled: bool) -> String {
    if !show_help {
        return String::new();
    }
    let ascii = cli.ascii || !styled;
    render::help(ascii, styled)
}

/// Compose the bottom of the frame in order: an optional status line (empty
/// unless a refresh failed), then (watch only) the `r refresh (next in 5m) - ?
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
/// (unreleased) version and the last few stable releases, with the labels
/// right-aligned so the colons and counts line up.
fn render_commits(f: &mut String, stats: &commits::CommitStats, styled: bool) {
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
        .chain(stats.releases.iter().map(|r| &r.count))
        .fold((0usize, false), |(n, capped), c| {
            (n + c.mine, capped || c.capped)
        });
    let total = format!("{total}{}", if capped { "+" } else { "" });
    f.push_str(&render::header(
        "My Shipments",
        status::TEAL,
        Some(&total),
        styled,
    ));
    f.push('\n');

    // (label, count) rows: the upcoming release first, then the shipped ones.
    let mut rows: Vec<(String, String)> = Vec::with_capacity(stats.releases.len() + 1);
    rows.push((
        "upcoming".to_string(),
        stats
            .upcoming
            .as_ref()
            .map_or_else(|| "\u{2014}".to_string(), &count),
    ));
    for r in &stats.releases {
        rows.push((r.tag.clone(), count(&r.count)));
    }

    let width = rows.iter().map(|(l, _)| l.width()).max().unwrap_or(0);
    for (i, (label, value)) in rows.iter().enumerate() {
        let pad = " ".repeat(width - label.width());
        // The first row is the upcoming (unreleased) version; set it apart in
        // italics. Plain (not dim): the counts are real data, not placeholders.
        let label = render::italic(label, styled && i == 0);
        f.push_str(&format!("  {pad}{label}: {value}\n"));
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
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    // Auth can drive the interactive device flow whenever there's a terminal,
    // but rendering is plain under `--once` (single-shot/scriptable output).
    let interactive = std::io::stdout().is_terminal();
    let styled = interactive && !cli.once;

    // `--demo`: render synthetic data once and exit (no auth/repo/network), so
    // the dashboard can be screenshotted. Styled on a TTY, plain when piped.
    if cli.demo {
        let sections = demo_sections();
        let changes = Changes {
            status_changed: std::collections::HashSet::from([127]),
            newly_merged: std::collections::HashSet::from([119]),
        };
        let next = timefmt::eta(cli.interval.dur);
        let body = render_body(&sections, &cli, &changes, interactive)
            + &bottom(
                "",
                &render::footer(&next, interactive),
                &help_block(&cli, !cli.no_help, interactive),
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

    // The next-refresh ETA is constant (the poll interval), so the key-hint
    // footer that carries it (`r refresh (next in 5m) - ? help`) is built once.
    let footer = render::footer(&timefmt::eta(cli.interval.dur), styled);

    // Change-detection / last-good state, seeded from the cache below so the
    // first refresh can highlight what changed while prowl wasn't running.
    let mut prev: Option<Tracker> = None;
    let mut last_good: Option<Sections> = None;
    // The help legend starts hidden and is toggled live with `?`. `last_status`
    // is the most recent trailing line (empty unless a refresh failed), reused
    // so a `?` toggle or `r` repaint keeps any error line on screen.
    let mut show_help = false;
    let mut last_status = String::new();

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
        match (!cli.no_cache).then(|| cache::load(&repo)).flatten() {
            Some(c) => {
                let body = render_body(&c.sections, &cli, &Changes::default(), styled)
                    + &bottom("", &footer, &help_block(&cli, show_help, styled));
                repaint(&body)?;
                prev = Some(Tracker::build(
                    c.sections.prs.as_deref(),
                    c.sections.merged.as_deref(),
                ));
                last_good = Some(c.sections);
            }
            None => {
                println!("{}{}", render::clear(), render::loading(styled));
                std::io::stdout().flush()?;
            }
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

    // Single render: --once, or whenever stdout is not a TTY.
    if cli.once || !styled {
        let sections = fetch(&cli, &client, &repo, &me, &default_branch)?;
        if !cli.no_cache {
            cache::save(&repo, &sections);
        }
        let frame = render_body(&sections, &cli, &Changes::default(), styled)
            + &bottom("", "", &help_block(&cli, !cli.no_help, styled));
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
        // Run the blocking fetch on a worker thread and poll input while it
        // runs, so `?` still toggles the help legend mid-refresh. Ticks and `r`
        // are ignored here — a refresh is already in flight.
        let result = std::thread::scope(|scope| {
            let handle = scope.spawn(|| fetch(&cli, &client, &repo, &me, &default_branch));
            while !handle.is_finished() {
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(60);
                if let term::Wait::ToggleHelp = term::wait(deadline) {
                    show_help = !show_help;
                    if let Some(good) = &last_good {
                        let body = render_body(good, &cli, &Changes::default(), styled)
                            + &bottom(&last_status, &footer, &help_block(&cli, show_help, styled));
                        let _ = repaint(&body);
                    }
                }
            }
            handle.join().expect("fetch thread panicked")
        });
        match result {
            Ok(sections) => {
                let tracker = Tracker::build(sections.prs.as_deref(), sections.merged.as_deref());
                let changes = prev.as_ref().map(|p| tracker.diff(p)).unwrap_or_default();
                let bell = changes.any();

                last_status = String::new();
                let body = render_body(&sections, &cli, &changes, styled)
                    + &bottom("", &footer, &help_block(&cli, show_help, styled));
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
                last_status = error_trailing(&short_error(&e), styled);
                let (main, help) = match &last_good {
                    Some(good) => (
                        render_body(good, &cli, &Changes::default(), styled),
                        help_block(&cli, show_help, styled),
                    ),
                    None => (String::new(), String::new()),
                };
                let body = main + &bottom(&last_status, &footer, &help);
                repaint(&body)?;
            }
        }
        // Wait for the interval, but let the user act now: `r` forces a refresh
        // (the worker thread then re-fetches with `?` still live), `?` toggles
        // the help legend in place; all other keys are discarded.
        let deadline = std::time::Instant::now() + cli.interval.dur;
        loop {
            match term::wait(deadline) {
                term::Wait::Tick | term::Wait::Refresh => break,
                term::Wait::ToggleHelp => {
                    show_help = !show_help;
                    if let Some(good) = &last_good {
                        let body = render_body(good, &cli, &Changes::default(), styled)
                            + &bottom(&last_status, &footer, &help_block(&cli, show_help, styled));
                        repaint(&body)?;
                    }
                }
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
            merged: Some(vec![]),
            commits: None,
        };
        let body = render_body(&sections, &cli, &Changes::default(), false);

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
}

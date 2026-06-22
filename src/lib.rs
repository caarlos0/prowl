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
pub mod timefmt;

use anyhow::{Context, Result};
use changes::{Changes, Tracker};
use clap::Parser;
use cli::Cli;
use github::{Client, Repo};
use std::io::Write;
use std::time::{Duration, Instant};
use uncurses::buffer::{Bounded, SurfaceMut, TextBuffer};
use uncurses::color::{Color, Profile};
use uncurses::event::Event;
use uncurses::screen::Screen;
use uncurses::style::Style;
use uncurses::terminal::{Stdin, Stdout, Terminal};
use uncurses::text::{Encode, TextSurface};

/// A fetched snapshot of every enabled section (`None` = section disabled).
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Sections {
    merged: Option<Vec<merged::MergedRow>>,
    queue: Option<Vec<queue::QueueRow>>,
    prs: Option<Vec<prs::PrRow>>,
    commits: Option<commits::CommitStats>,
}

impl Sections {
    /// Every section disabled — painted as just the bottom (error/footer/help)
    /// when a fetch fails before any data has arrived.
    const EMPTY: Sections = Sections {
        merged: None,
        queue: None,
        prs: None,
        commits: None,
    };
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
            "build(deps): bump uncurses to 0.2",
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

/// Paint one PR section onto `s` at row `top`: a counted header, then either its
/// table or, when empty, a dim placeholder, then a trailing blank row. Returns
/// the next free row.
#[allow(clippy::too_many_arguments)]
fn paint_section(
    s: &mut impl TextSurface,
    title: &str,
    accent: Color,
    count: usize,
    empty_msg: &str,
    table: Option<&render::Table>,
    title_w: usize,
    ascii: bool,
    top: u16,
) -> u16 {
    let y = render::paint_header(s, title, accent, Some(&count.to_string()), ascii, top);
    let y = match table {
        Some(table) => render::paint_table(s, table, title_w, ascii, y),
        None => render::paint_dim(s, empty_msg, y),
    };
    y + 1
}

/// Paint the whole dashboard onto `s` starting at row 0: My open PRs, Merge
/// Queue, My merged PRs, My Shipments, then the optional `error:` line, footer,
/// and help legend. Each PR section always shows its header (with a count); an
/// empty section follows it with a dim placeholder. Rows that changed since the
/// previous refresh (per `changes`) are flagged with a leading marker.
///
/// `ascii` selects letters/parens over Nerd Font glyphs/bars; colors are written
/// as styles and downsampled by the surface's `Profile` at encode/render time.
/// Returns the number of rows used.
#[allow(clippy::too_many_arguments)]
fn paint_dashboard(
    s: &mut impl TextSurface,
    sections: &Sections,
    changes: &Changes,
    error: &str,
    footer_eta: Option<&str>,
    show_help: bool,
    ascii: bool,
) -> u16 {
    let prs_table = sections
        .prs
        .as_ref()
        .filter(|r| !r.is_empty())
        .map(|rows| prs::to_table(rows, ascii, &changes.status_changed));
    let queue_table = sections
        .queue
        .as_ref()
        .filter(|r| !r.is_empty())
        .map(|rows| queue::to_table(rows, ascii));
    let merged_table = sections
        .merged
        .as_ref()
        .filter(|r| !r.is_empty())
        .map(|rows| merged::to_table(rows, ascii, &changes.newly_merged));

    // The shared TITLE width keeps the tables aligned and the view within
    // MAX_WIDTH; pass it to every section so the columns line up.
    let title_w = {
        let tables: Vec<&render::Table> = [
            prs_table.as_ref(),
            queue_table.as_ref(),
            merged_table.as_ref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        render::title_width(s, &tables)
    };

    // Sections in display order: (row count, table, title, accent, empty text).
    // A `None` count means the section is disabled and is skipped entirely.
    let plan = [
        (
            sections.prs.as_ref().map(|r| r.len()),
            prs_table.as_ref(),
            "My open PRs",
            status::LAVENDER,
            "No open PRs.",
        ),
        (
            sections.queue.as_ref().map(|r| r.len()),
            queue_table.as_ref(),
            "Merge Queue",
            status::BLUE,
            "No merge queue.",
        ),
        (
            sections.merged.as_ref().map(|r| r.len()),
            merged_table.as_ref(),
            "My merged PRs",
            status::MAUVE,
            "No recent merged PRs.",
        ),
    ];

    let mut y = 0u16;
    for (count, table, title, accent, empty) in plan {
        if let Some(n) = count {
            y = paint_section(s, title, accent, n, empty, table, title_w, ascii, y);
        }
    }
    if let Some(stats) = &sections.commits {
        y = paint_commits(s, stats, ascii, y) + 1;
    }

    // Bottom: error line, footer, help — each separated from the previous part
    // by one blank row (the body's trailing blank serves as the first).
    let mut painted = false;
    if !error.is_empty() {
        y = render::paint_dim(s, &format!("error: {error}"), y);
        painted = true;
    }
    if let Some(eta) = footer_eta {
        if painted {
            y += 1;
        }
        y = render::paint_footer(s, eta, ascii, y);
        painted = true;
    }
    if show_help {
        if painted {
            y += 1;
        }
        y = render::paint_help(s, ascii, y);
    }
    y
}

/// A safe upper bound on the dashboard height, used to size a surface before it
/// is cropped to the painted height.
fn height_bound(s: &Sections, show_help: bool) -> u16 {
    let mut n = 8usize; // error + footer + slack
    n += s.prs.as_ref().map_or(0, |r| r.len() + 3);
    n += s.queue.as_ref().map_or(0, |r| r.len() + 3);
    n += s.merged.as_ref().map_or(0, |r| r.len() + 3);
    n += s.commits.as_ref().map_or(0, |c| c.releases.len() + 4);
    if show_help {
        n += status::ORDER.len() + status::STATE_ORDER.len() + 4;
    }
    n as u16
}

/// Paint the one-row `Loading...` startup frame (a single dim line) and render it.
/// Shared by the watch's first paint when there's no cache and by interactive
/// `--once`, so both show the identical loading frame.
fn paint_loading(screen: &mut Screen<Stdin, Stdout>) -> Result<()> {
    screen.resize((screen.width().max(1), 1));
    screen.clear();
    render::paint_dim(screen, "Loading...", 0);
    screen.render()?;
    Ok(())
}

/// Size an inline/alternate `Screen` to the dashboard's content height, paint it,
/// crop to the height actually used, and render. Shared by the watch redraw and
/// the interactive one-shot frame so the sizing dance lives in one place.
fn render_dashboard(
    screen: &mut Screen<Stdin, Stdout>,
    sections: &Sections,
    changes: &Changes,
    error: &str,
    footer_eta: Option<&str>,
    show_help: bool,
    ascii: bool,
) -> Result<()> {
    let w = screen.width().max(1);
    // Grow tall enough to paint everything, paint, then shrink to the height
    // actually used so the surface is exactly the dashboard's line count.
    screen.resize((w, height_bound(sections, show_help).max(1)));
    screen.clear();
    let used = paint_dashboard(
        screen, sections, changes, error, footer_eta, show_help, ascii,
    );
    screen.resize((w, used.max(1)));
    screen.render()?;
    Ok(())
}

/// Render the dashboard once into an offscreen [`TextBuffer`] sized to its content,
/// then encode it to the terminal's output with the **detected** color profile
/// (plain when piped) and exit. Used by `--once`, non-TTY output, and `--demo`.
fn render_once(
    terminal: &Terminal<Stdin, Stdout>,
    sections: &Sections,
    cli: &Cli,
    changes: &Changes,
    footer_eta: Option<&str>,
) -> Result<()> {
    let profile = Profile::detect_from(terminal.env(), terminal.is_terminal().1);
    let ascii = cli.ascii || profile == Profile::Disabled;
    let show_help = !cli.no_help;

    let w = render::MAX_WIDTH as u16;
    let mut canvas = TextBuffer::new(w, height_bound(sections, show_help));
    let used = paint_dashboard(
        &mut canvas,
        sections,
        changes,
        "",
        footer_eta,
        show_help,
        ascii,
    );
    canvas.resize(w, used.max(1));

    // A closed downstream pipe (`prowl --once | head`) is a clean exit, not an
    // error worth printing.
    let mut out = terminal.output();
    let write = canvas
        .encode_with(&mut out, profile)
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush());
    match write {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Interactive `--once`: bring up an inline `Screen` (raw mode, hidden cursor, no
/// echo) showing a `Loading...` frame while the fetch runs on a background
/// thread, so keystrokes never echo and `q`/`Esc`/`Ctrl-C` can abort mid-fetch.
/// On success the dashboard replaces the loading frame and is left inline in the
/// terminal (like piped `--once`); on abort the frame is wiped and nothing is
/// left behind. `Screen::finish` restores the terminal on every path.
fn run_once_interactive(
    terminal: Terminal<Stdin, Stdout>,
    cli: &Cli,
    client: &Client,
    repo: &Repo,
) -> Result<()> {
    let mut screen = Screen::new(terminal)?;
    screen.init()?;
    screen.hide_cursor()?;

    // Inline loading frame; raw mode swallows keystrokes so nothing echoes into
    // the output while we wait.
    paint_loading(&mut screen)?;

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
            fetch(&cli2, &client2, &repo2, &me, &default_branch)
        })();
        let _ = tx.send(fetched); // ignored if we already aborted (rx dropped)
    });

    // `None` => the user aborted; `Some(result)` => the fetch finished.
    let fetched = loop {
        match rx.try_recv() {
            Ok(result) => break Some(result),
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                break Some(Err(anyhow::anyhow!("fetch worker stopped unexpectedly")));
            }
        }
        if screen.poll_event(Some(Duration::from_millis(60)))? {
            let mut aborted = false;
            while let Some(ev) = screen.try_read_event() {
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
            render_dashboard(
                &mut screen,
                &sections,
                &Changes::default(),
                "",
                None,
                !cli.no_help,
                cli.ascii,
            )?;
            screen.finish()?;
            if !cli.no_cache {
                cache::save(repo, &sections);
            }
            Ok(())
        }
        Some(Err(e)) => {
            screen.finish()?; // restore the terminal before surfacing the error
            Err(e)
        }
        None => {
            // Aborted: wipe the loading frame so nothing is left behind.
            screen.clear();
            screen.render()?;
            screen.finish()?;
            Ok(())
        }
    }
}

/// Paint the "My Shipments" section onto `s` at row `top`: my commit counts for
/// the next (unreleased) version and the last few stable releases, with the
/// labels right-aligned so the colons and counts line up. Returns the next row.
fn paint_commits(
    s: &mut impl TextSurface,
    stats: &commits::CommitStats,
    ascii: bool,
    top: u16,
) -> u16 {
    if !stats.available {
        return render::paint_dim(s, "Commit stats unavailable.", top);
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
    let mut y = render::paint_header(s, "My Shipments", status::TEAL, Some(&total), ascii, top);

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

    let labelw = rows
        .iter()
        .map(|(l, _)| s.str_width(l) as usize)
        .max()
        .unwrap_or(0);
    for (i, (label, value)) in rows.iter().enumerate() {
        // Right-align the labels in a two-space-indented column.
        let x = 2 + labelw.saturating_sub(s.str_width(label) as usize);
        // The upcoming (unreleased) version is set apart in italics; counts stay plain.
        let style = if i == 0 && !ascii {
            Style::new().italic()
        } else {
            Style::new()
        };
        let p = s.set_str((x as u16, y), label, style);
        s.set_str((p.x, y), &format!(": {value}"), None);
        y += 1;
    }
    y
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

/// What a keypress or resize means to the watch loop.
enum Action {
    /// Ignore (an unbound key, or a non-input event).
    None,
    /// `q`/`Esc`/`Ctrl-C`: quit.
    Quit,
    /// `r`/`R`: refresh now.
    Refresh,
    /// `?`: toggle the help legend.
    ToggleHelp,
    /// `ctrl+z`: suspend to the shell, then resume.
    Suspend,
    /// The terminal was resized to these cell dimensions.
    Resize(u16, u16),
}

/// Classify an event into a watch-loop [`Action`]. In raw mode the signal keys
/// arrive as ordinary key events, so `ctrl+c`/`ctrl+z` are matched here rather
/// than through signal handlers. `r` and `?` fold case via `Key::matches`.
fn classify(ev: &Event) -> Action {
    match ev {
        Event::KeyPress(k) => {
            if k.matches_any(["q", "esc", "ctrl+c"]) {
                Action::Quit
            } else if k.matches("r") {
                Action::Refresh
            } else if k.matches("?") {
                Action::ToggleHelp
            } else if k.matches("ctrl+z") {
                Action::Suspend
            } else {
                Action::None
            }
        }
        Event::Resize(ws) => Action::Resize(ws.col, ws.row),
        _ => Action::None,
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

/// Entry point: authenticate, resolve repo + user, then render once or watch.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    // Detect interactivity through uncurses' `Terminal` (is the output half a
    // TTY?) and reuse the very same handle to build the watch `Screen` or to
    // encode the one-shot frame. Auth can drive the interactive device flow
    // whenever there's a terminal.
    let terminal = Terminal::stdio();
    let interactive = terminal.is_terminal().1;

    // `--demo`: render synthetic data once and exit (no auth/repo/network), so
    // the dashboard can be screenshotted. Colored on a TTY, plain when piped.
    if cli.demo {
        let sections = demo_sections();
        let changes = Changes {
            status_changed: std::collections::HashSet::from([127]),
            newly_merged: std::collections::HashSet::from([119]),
        };
        let next = timefmt::eta(cli.interval.dur);
        return render_once(&terminal, &sections, &cli, &changes, Some(&next));
    }

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
        let sections = fetch(&cli, &client, &repo, &me, &default_branch)?;
        if !cli.no_cache {
            cache::save(&repo, &sections);
        }
        return render_once(&terminal, &sections, &cli, &Changes::default(), None);
    }

    // Interactive `--once`: an inline screen shows a `Loading...` frame and
    // swallows input while the fetch runs (abortable with `q`), then leaves the
    // dashboard in the terminal.
    if cli.once {
        return run_once_interactive(terminal, &cli, &client, &repo);
    }

    // Interactive watch, structured as an uncurses `App` (start → run → stop):
    // `stop` always runs, so `Screen::finish` restores the terminal on every
    // path — a clean quit, a `?`-operator error, or a panic-free fall-through.
    let mut app = App::start(terminal, &cli, &client, &repo)?;
    let result = app.run();
    app.stop()?;
    result
}

/// The interactive watch, following the uncurses example `App` pattern: it owns
/// the `Screen` and all dashboard state. `start` brings the terminal up, `run`
/// drives the refresh + event loop (returning `Ok(())` when a quit key is
/// pressed), and `stop` tears it back down with `Screen::finish`. The caller
/// always calls `stop`, so the terminal is restored on every path.
struct App<'a> {
    screen: Screen<Stdin, Stdout>,
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
    /// Help legend visibility (toggled with `?`) and the most recent short error
    /// (empty unless a refresh failed), reused so a `?` toggle keeps it on screen.
    show_help: bool,
    last_error: String,
    /// Whether the bell is armed. The first refresh after a cached start is
    /// silent (it still highlights changes).
    armed: bool,
    /// Whether we've switched from the inline loading frame to the alternate
    /// screen. The watch starts inline and enters the alt screen once the first
    /// fetch lands (or immediately when there's a cache to paint).
    in_alt: bool,
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
        let mut screen = Screen::new(terminal)?;
        screen.init()?;
        screen.hide_cursor()?;

        let mut app = App {
            eta: timefmt::eta(cli.interval.dur),
            screen,
            cli,
            client,
            repo,
            me: String::new(),
            default_branch: String::new(),
            prev: None,
            last_good: None,
            show_help: false,
            last_error: String::new(),
            armed: false,
            in_alt: false,
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
            .then(|| cache::load(self.repo))
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
            None => paint_loading(&mut self.screen)?,
        }
        Ok(())
    }

    /// Switch from the inline loading frame to the alternate screen, once. The
    /// inline frame is dropped to zero rows and flushed first, so taking over the
    /// screen leaves the terminal as it was before prowl ran.
    fn enter_alt(&mut self) -> Result<()> {
        if !self.in_alt {
            self.screen.resize((self.screen.width().max(1), 0));
            self.screen.render()?;
            self.screen.enter_alt_screen()?;
            self.in_alt = true;
        }
        Ok(())
    }

    /// Paint the current dashboard via [`render_dashboard`], drawing the last
    /// good sections (or an empty frame, so a first-fetch error still shows its
    /// error line + footer) with `changes` highlighted.
    fn redraw(&mut self, changes: &Changes) -> Result<()> {
        let sections = self.last_good.as_ref().unwrap_or(&Sections::EMPTY);
        render_dashboard(
            &mut self.screen,
            sections,
            changes,
            &self.last_error,
            Some(&self.eta),
            self.show_help,
            self.cli.ascii,
        )
    }

    /// Drive the watch: loop fetch → paint → wait, returning `Ok(())` when the
    /// user presses a quit key.
    fn run(&mut self) -> Result<()> {
        loop {
            if let Flow::Quit = self.fetch_responsive()? {
                return Ok(());
            }
            if let Flow::Quit = self.wait_interval()? {
                return Ok(());
            }
        }
    }

    /// Tear the terminal back down. The consuming `Screen::finish` is the
    /// idiomatic teardown: it exits the alternate screen, shows the cursor, and
    /// leaves raw mode.
    fn stop(self) -> Result<()> {
        self.screen.finish()?;
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
                let sections = fetch(&cli, &client, &repo, &me, &default_branch)?;
                Ok((me, default_branch, sections))
            })();
            let _ = tx.send(fetched); // ignored if we already quit (rx dropped)
        });

        loop {
            match rx.try_recv() {
                Ok(Ok((me, default_branch, sections))) => {
                    self.me = me;
                    self.default_branch = default_branch;
                    self.apply(sections)?;
                    return Ok(Flow::Continue);
                }
                Ok(Err(e)) => {
                    self.show_error(e)?;
                    return Ok(Flow::Continue);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(Flow::Continue),
            }
            if self.screen.poll_event(Some(Duration::from_millis(60)))? {
                while let Some(ev) = self.screen.try_read_event() {
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
            if !self.screen.poll_event(Some(remaining))? {
                break; // timed out: scheduled refresh
            }
            while let Some(ev) = self.screen.try_read_event() {
                match self.handle_event(&ev)? {
                    Flow::Quit => return Ok(Flow::Quit),
                    Flow::Refresh => return Ok(Flow::Continue), // refresh now
                    Flow::Continue => {}
                }
            }
        }
        Ok(Flow::Continue)
    }

    /// Apply an input event's side effects (suspend, help toggle, resize repaint)
    /// and report the control flow it implies.
    fn handle_event(&mut self, ev: &Event) -> Result<Flow> {
        Ok(match classify(ev) {
            Action::Quit => Flow::Quit,
            Action::Refresh => Flow::Refresh,
            Action::Suspend => {
                self.screen.suspend()?;
                self.screen.resume()?;
                // Repaint after coming back from the shell — the canvas may not
                // survive the suspend, so don't rely on `resume`'s flush.
                self.repaint_last()?;
                Flow::Continue
            }
            Action::ToggleHelp => {
                self.show_help = !self.show_help;
                self.repaint_last()?;
                Flow::Continue
            }
            Action::Resize(w, h) => {
                self.screen.resize((w, h));
                self.repaint_last()?;
                Flow::Continue
            }
            Action::None => Flow::Continue,
        })
    }

    /// Render a successful fetch: diff against the previous snapshot, paint, ring
    /// the bell on a change (once armed), and cache the result.
    fn apply(&mut self, sections: Sections) -> Result<()> {
        let tracker = Tracker::build(sections.prs.as_deref(), sections.merged.as_deref());
        let changes = self
            .prev
            .as_ref()
            .map(|p| tracker.diff(p))
            .unwrap_or_default();
        let bell = changes.any();

        self.last_error.clear();
        self.prev = Some(tracker);
        self.last_good = Some(sections);
        self.enter_alt()?;
        self.redraw(&changes)?;

        if self.armed && bell && !self.cli.no_bell {
            let _ = self.screen.beep();
        }
        self.armed = true;
        if !self.cli.no_cache
            && let Some(good) = &self.last_good
        {
            cache::save(self.repo, good);
        }
        Ok(())
    }

    /// Render a failed fetch: keep the last good data, add a dim error line, and
    /// do not ring. With no data yet, just the error line and footer show.
    fn show_error(&mut self, e: anyhow::Error) -> Result<()> {
        self.last_error = short_error(&e);
        self.enter_alt()?;
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
    use uncurses::text::Encode;

    #[test]
    fn empty_sections_still_show_their_headers_then_a_placeholder() {
        let sections = Sections {
            prs: Some(vec![]),
            queue: Some(vec![]),
            merged: Some(vec![]),
            commits: None,
        };
        let mut canvas = TextBuffer::new(render::MAX_WIDTH as u16, 64);
        let used = paint_dashboard(
            &mut canvas,
            &sections,
            &Changes::default(),
            "",
            None,
            false,
            true,
        );
        canvas.resize(render::MAX_WIDTH as u16, used.max(1));
        let body = canvas.display_with(Profile::Disabled).to_string();

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

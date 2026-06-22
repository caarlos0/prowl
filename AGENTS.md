# AGENTS.md

Orientation for AI agents (and humans) working on **prowl**. Keep this file up
to date in the same change whenever the architecture, modules, queries, data
model, or workflow change.

## What prowl is

A small terminal dashboard that watches a GitHub repo and re-renders on an
interval: **My open PRs → Merge Queue → My merged PRs → My Shipments**, then a
`r refresh (next in 5m) - ? help` footer (which also shows the time until the
next refresh) and an optional help legend last at the bottom. It rings the
terminal bell when one of your PRs merges or
an open PR's status changes, and flags the changed rows. The interactive watch
runs on the [**uncurses**](https://github.com/aymanbagabas/uncurses) toolkit:
an alternate-screen [`Screen`] with an event loop. Interactive `--once` uses an
*inline* `Screen` instead: a `Loading...` frame while the fetch runs (abortable
with `q`), then the dashboard is left in the terminal. Piped/non-TTY/`--demo`
output is plain text printed straight to stdout, so the dashboard stays
pipe-friendly and URLs can be OSC-8 hyperlinks.

## Golden rules

- **Transport is the native GitHub API over HTTP** (`ureq` + rustls), not the
  `gh` CLI. `github::Client` sends a Bearer token with a User-Agent +
  `X-GitHub-Api-Version`. GraphQL is a `POST /graphql` with `{query, variables}`;
  parse the full `{"data":...}` envelope (`github::parse_graphql`, surfacing
  GraphQL `errors`). REST is `GET /<path>`.
- **Auth** lives in `auth.rs`: token resolution is `PROWL_TOKEN` → `GITHUB_TOKEN`
  → OS keyring / chmod-600 file → OAuth **device flow** (interactive). The OAuth
  App client id is public and embedded. `--login` forces the device flow.
- **The terminal toolkit is `uncurses`** (the author's own low-level library):
  its `style::Style` carries SGR + the OSC-8 link, the `Screen` facade owns raw
  mode / the alternate screen / input / teardown, and `text` provides width math.
  Don't reach for a higher-level TUI framework (ratatui, etc.): the watch is a
  full-repaint dashboard, and one-shot output must degrade to plain piped text.
- **Styling:** built on `uncurses::style::Style` (SGR incl. 24-bit truecolor;
  OSC-8 links ride in the style). There is **one painter**: the dashboard is
  drawn straight onto an `uncurses` surface with `set_str`. Plain-vs-styled is
  not a code branch — the surface's color `Profile` downsamples at encode/render
  time, and `Profile::Disabled` (non-TTY/piped) drops SGR and hyperlinks, so
  piped output is plain automatically. Glyph-vs-letter and bar-vs-parens are the
  one content choice, driven by an `ascii` flag (`--ascii`, or a `Disabled`
  profile).
- **One status palette.** Colors and glyphs live only in `status.rs` (Catppuccin
  Mocha + Nerd Font), as `uncurses::color::Color` constants. Don't redefine them.

## Layout (lib + thin bin)

`src/main.rs` is a thin binary calling `prowl::run()`. `src/lib.rs` orchestrates
(painting the dashboard onto a surface, encoding the one-shot frame, and the
watch event loop); everything else is testable modules:

- `cli.rs` — clap derive CLI, `Section` enum, duration parser (`s/m/h/d/w`).
- `github.rs` — `Client` (HTTP `graphql()`/`get()`), `Repo`, `me()`,
  `default_branch()`, `detect_repo()` (parses the git `origin` remote),
  `parse_graphql()`.
- `auth.rs` — device-flow login + token storage (keyring/file).
- `model.rs` — serde structs + `fetch_*` for the three queries; query strings.
- `status.rs` — **the** palette: `Status`, `status_style` (returns a glyph +
  `Color`), glyphs/ASCII, `derive_status` (precedence), `fail_count`; and the
  `mergeStateStatus` helpers `state_style`, `state_label` (DIRTY → CONFLICTS),
  `state_glyph`, `state_meaning`. `fg(Color)` builds the foreground `Style`.
- `render.rs` — the surface painters: `paint_table`/`paint_header`/`paint_dim`/
  `paint_footer`/`paint_help` write onto any `&mut impl TextSurface` using the
  surface's own `str_width` (no in-house width math) and `set_str` (column gaps
  are implicit — unpainted cells stay blank, so no padding is emitted). `Cell`
  (text + `Style`, the OSC-8 link folded into the style) / `Table`, `truncate`
  (uncurses' width-aware truncator), and `title_width` (cap/align the shared
  `TITLE` column so every table lines up and the whole view stays within
  `MAX_WIDTH` = 120). Headers, the key-hint footer (carrying the next-refresh
  ETA), and the help legend live here too.
- `queue.rs` / `prs.rs` / `merged.rs` — per-section rows, sorting, `to_table`.
  Each row's PR number is the OSC-8 link (no separate URL column); the queue
  columns are `# PR TITLE AUTHOR` (author truncated to `AUTHOR_WIDTH`).
- `commits.rs` — "commits by me" counts for the next (unreleased) version and
  the last 4 stable releases (GitHub releases + compare REST APIs); best-effort,
  never fatal. Rendered as the right-aligned "My Shipments" section.
- `changes.rs` — `Tracker`/`Changes`: bell + highlight detection.
- `cache.rs` — per-repo on-disk cache of the last `Sections` under
  `$XDG_CACHE_HOME/prowl` (so the watch dashboard paints instantly on startup).
- `timefmt.rs` — `chrono` helpers (local clock, `mergedAt` ages, since-date).

`run()` first creates a `uncurses::terminal::Terminal::stdio()`; interactivity is
its `is_terminal().1` (output a TTY?). When the output is **not** a TTY (piped,
redirected) and for `--demo`, `render_once` paints the dashboard onto an offscreen
`TextBuffer` sized to its content (a generous `height_bound`, then cropped to the
painted height), and `encode_with`s it to the terminal's output (`Terminal::output`)
using the **detected** color `Profile` (`Profile::detect_from`), so it's colored on
a TTY and plain when piped. Interactive `--once` instead runs `run_once_interactive`:
an *inline* `Screen` (raw mode, hidden cursor) shows a `Loading...` frame while the
fetch runs on a background thread, so keystrokes don't echo and `q`/`Esc`/`Ctrl-C`
aborts mid-fetch; on success the dashboard replaces the frame and is left inline
(`Screen::finish` doesn't wipe an inline surface). Otherwise the same `Terminal` is
moved into `App::start` → `Screen::new(terminal)`. The watch redraw and the inline
one-shot frame share `render_dashboard`, which sizes the surface to the content,
paints, crops to the painted height, and renders.

The interactive watch is `lib.rs::App`, following the uncurses example **`App`
pattern**: the struct owns the `uncurses::Screen` plus all dashboard state, and
`run()` does `let mut app = App::start(terminal, ...)?; let result = app.run();
app.stop()?; result`. `start` builds the screen from the `Terminal`, brings it up
(alt-screen, hidden cursor, keeping the terminal's detected color profile) and
paints the
cache/loading frame; `run` resolves `me`/default branch then loops fetch → paint
→ wait, returning `Ok(())` on a quit key; `stop` consumes the app and calls
**`Screen::finish`** (the idiomatic teardown: exit alt-screen, show cursor, leave
raw mode). Because the caller always runs `stop`, the terminal is restored on
every path — a clean quit, a `?`-operator error, or a failed first paint (`start`
calls `stop` itself before bailing). Each frame is painted by `redraw` →
`render_dashboard`, which **resizes the surface to the exact content height**
(even in the alternate screen) before `render`. The loop uses `poll_event` with
the interval as the timeout. Keys are matched with `Key::matches`: `r`/`R` refresh
now, `?` toggles help, `q`/`Esc`/`Ctrl-C` quit, `Ctrl-Z` suspends/resumes,
`Resize` repaints.

## Key behaviors

- **Status precedence:** `merged > conflicts > fail > pending > pass > none`.
  Check suites with **zero check runs** (`checkRuns.totalCount == 0`) are
  phantom and ignored for both the glyph and the `FAIL` count, matching GitHub's
  rollup (so a `CLEAN` PR stays green).
- **Sorting:** open and merged PRs by `updatedAt` desc; queue by `position` asc.
- **Bell:** rings once per refresh when a PR of mine merges or an open PR's
  status changes (keyed by PR number, so re-sorting / new PRs / title edits do
  not ring). The first refresh is silent. Changed rows get a `▸` marker.
- **Resilience:** a failed API call keeps the last good data, shows a dim error
  line, and does not ring.
- **Cache:** on a watch start, prowl paints the cached `Sections` immediately,
  seeds change-detection from it
  so the first live refresh highlights what changed while prowl wasn't running,
  but stays silent (no startup bell). `--no-cache` skips both read and write.
- **Terminal:** the watch runs on a `uncurses::Screen` in the alternate screen
  with the cursor hidden; raw mode means stray keystrokes never garble the
  dashboard or spill into the shell. `r`/`R` forces a refresh now; `?` toggles
  the help legend (a full static reference of every status glyph + `STATE` value,
  hidden by default, rendered last at the very bottom; `--no-help` only affects
  one-shot/piped output); `q`/`Esc`/`Ctrl-C` quit and `Ctrl-Z` suspends/resumes.
  The only persistent bottom line is the footer
  (`r refresh (next in 5m) - ? help`), which carries the next-refresh ETA; a
  failed refresh adds a dim `error: …` line above it. Every fetch (and the
  one-time `me`/default-branch resolution) runs on a **detached background
  thread** and returns over a channel; the main thread only polls input and
  paints, so network I/O never blocks the UI — `?`/resize/suspend stay live
  mid-refresh and **quit is instant** (a quit abandons the in-flight request,
  which is reaped at process exit). The terminal is restored on every exit path
  by `App::stop` (`Screen::finish`), which the caller always runs after
  `App::run`.
- **Interactive `--once`:** `run_once_interactive` brings up an *inline* `Screen`
  (raw mode, hidden cursor) and paints a `Loading...` frame while the fetch runs on
  a background thread, so keystrokes don't echo and `q`/`Esc`/`Ctrl-C` aborts the
  fetch instantly. On success the dashboard replaces the frame and is left inline in
  the terminal; on abort the frame is wiped. `Screen::finish` restores the terminal
  on every path. Piped/non-TTY output keeps the plain `render_once` encode path.

## The three GraphQL queries + REST (see `model.rs` / `commits.rs`)

- Merge queue: `repository.mergeQueue.entries` (vars `owner`, `name`).
- Open PRs: `search(is:pr is:open author:<me>)` with `mergeable`,
  `mergeStateStatus`, `mergeQueueEntry`, last commit `checkSuites { conclusion
  checkRuns { totalCount } }`, `updatedAt`.
- Merged: `search(is:pr is:merged author:<me> merged:>=<since>)` with `mergedAt`,
  `updatedAt`, `baseRefName`.
- Commits section: REST `GET /repos/.../releases`, `/compare/a...b`, `/commits`.

## Build / test / lint

```sh
cargo build                                  # must be warning-free
cargo clippy --all-targets -- -D warnings    # must be clean
cargo fmt --all --check                      # must be formatted
cargo test                                   # offline, fixture-based
```

CI (`.github/workflows/build.yml`) runs fmt/clippy/build/test on push and PRs.

## Releases

Tag `vX.Y.Z` → `.github/workflows/release.yml` runs **GoReleaser Pro**
(`.goreleaser.yaml`). The config `includes:` shared snippets from
[`caarlos0/goreleaserfiles`](https://github.com/caarlos0/goreleaserfiles)
(changelog/release, notarization, packaging) and publishes: archives, nfpm/nix/
homebrew-cask packages, the npm package `@caarlos0/prowl`, SBOMs, and a
cosign-signed checksum. `snapshot.yml` builds a snapshot on pushes/same-repo PRs.
Required secrets: `GORELEASER_KEY`, `GH_PAT` (repo scope, for tap/nur pushes),
`NPM_TOKEN`; `MACOS_*` enable optional macOS notarization.

Tests are offline: JSON fixtures under `tests/fixtures/` (real captures + a
crafted queue) drive parsing → rows → render in `tests/parsing.rs`, plus
per-module unit tests. No network in tests.

## Conventions

Conventional Commits with scope, one logical change per commit, signed off
(`git commit -s`). Merge (never rebase) when integrating `main`. Keep it simple;
verify before fixing. See `CONTRIBUTING.md`.

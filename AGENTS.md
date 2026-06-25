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
an open PR's status changes, and flags the changed rows. It is a plain
`std::thread::sleep` redraw loop — **not** a raw-mode/alt-screen TUI — so output
stays pipe-friendly and URLs can be OSC-8 hyperlinks.

## Golden rules

- **Transport is the native GitHub API over HTTP** (`ureq` + rustls), not the
  `gh` CLI. `github::Client` sends a Bearer token with a User-Agent +
  `X-GitHub-Api-Version`. GraphQL is a `POST /graphql` with `{query, variables}`;
  parse the full `{"data":...}` envelope (`github::parse_graphql`, surfacing
  GraphQL `errors`). REST is `GET /<path>`.
- **Auth** lives in `auth.rs`: token resolution is `PROWL_TOKEN` → `GITHUB_TOKEN`
  → OS keyring / chmod-600 file → OAuth **device flow** (interactive). The OAuth
  App client id is public and embedded. `--login` forces the device flow.
- **Don't add a TUI framework** (ratatui, etc.): it cannot emit OSC-8 hyperlinks
  and does not degrade to plain text when piped. Both are required.
- **Styling:** `anstyle` for SGR incl. 24-bit truecolor; OSC-8 links, the bell,
  and the screen clear are emitted by hand. All of it is gated on a `styled`
  flag, so output is plain when piped, on a non-TTY, or with `--once`, and styled
  only on an interactive TTY watch. A false `styled` flag drops the SGR colors,
  OSC-8 hyperlinks, glyphs, and the clear, leaving plain ASCII.
- **One status palette.** Colors and glyphs live only in `status.rs` (Catppuccin
  Mocha + Nerd Font). Don't redefine them elsewhere.

## Layout (lib + thin bin)

`src/main.rs` is a thin binary calling `prowl::run()`. `src/lib.rs` orchestrates;
everything else is testable modules:

- `cli.rs` — clap derive CLI, `Section` enum, duration parser (`s/m/h/d/w`).
- `github.rs` — `Client` (HTTP `graphql()`/`get()`), `Repo`, `me()`,
  `default_branch()`, `detect_repo()` (parses the git `origin` remote),
  `parse_graphql()`.
- `auth.rs` — device-flow login + token storage (keyring/file).
- `model.rs` — serde structs + `fetch_*` for the three queries; query strings.
- `status.rs` — **the** palette: `Status`, `status_style`, glyphs/ASCII,
  `derive_status` (precedence), `fail_count`; and the `mergeStateStatus`
  helpers `state_style`, `state_label` (DIRTY → CONFLICTS), `state_glyph`,
  `state_meaning`.
- `render.rs` — `Cell`/`Table`, width-aware padding (`unicode-width`), OSC-8
  (incl. `link_styled` for clickable PR numbers), `truncate` + `fit_titles`
  (cap/align the shared `TITLE` column so every table lines up and the whole
  view stays within `MAX_WIDTH` = 120 columns), headers, the key-hint footer
  (`footer`, carrying the relative next-refresh ETA), help legend (a full static
  reference of every status glyph + `STATE`
  value, last at the very bottom), loading screen, bell, clear.
- `queue.rs` / `prs.rs` / `merged.rs` — per-section rows, sorting, `to_table`.
  Each row's PR number is the OSC-8 link (no separate URL column); the queue
  columns are `# PR TITLE AUTHOR` (author truncated to `AUTHOR_WIDTH`).
- `commits.rs` — "commits by me" counts for the next (unreleased) version and
  the last 4 stable releases (GitHub releases + compare REST APIs); best-effort,
  never fatal. Rendered as the right-aligned "My Shipments" section.
  `--include-pre-releases` also counts prereleases (drafts are always skipped).
- `changes.rs` — `Tracker`/`Changes`: bell + highlight detection.
- `cache.rs` — per-repo on-disk cache of the last `Sections` under
  `$XDG_CACHE_HOME/prowl` (so the watch dashboard paints instantly on startup).
- `term.rs` — Unix terminal helper: while watching, quiet stdin (drop echo +
  line buffering, keep `ISIG` so signal keys work) and turn the interval wait
  into a poll, so `r` refreshes now and `?` toggles the help legend, while every
  other key is discarded; restored on every exit path. A no-op on non-Unix.
- `timefmt.rs` — `chrono` helpers (local clock, `mergedAt` ages, since-date).

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
- **Terminal:** while watching, the cursor is hidden and stdin echo/line
  buffering are turned off, so stray keystrokes neither garble the dashboard nor
  spill into the shell; signal keys (Ctrl-C/Ctrl-Z) still fire. `r`/`R` forces a
  refresh now; `?` toggles the help legend
  (a full static reference of every status glyph + `STATE` value, hidden by
  default, rendered last at the very bottom; `--no-help` only affects
  one-shot/piped output). The only persistent bottom line is the footer
  (`r refresh (next in 5m) - ? help`), which carries the next-refresh ETA; a
  failed refresh adds a dim `error: …` line above it. The blocking fetch runs on
  a worker thread (`std::thread::scope`) while the main thread keeps polling
  input, so `?` stays responsive even mid-refresh. Both the cursor and terminal
  mode are
  restored on every normal or early (`?`-operator) return (Drop guards) and on
  SIGINT (the Ctrl-C handler).

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

The hidden `--demo` flag (synthetic data for screenshots) is behind the
off-by-default `demo` cargo feature, so release builds don't ship it. Build or
run it with `cargo run --features demo -- --demo`.

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

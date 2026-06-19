# AGENTS.md

Orientation for AI agents (and humans) working on **prowl**. Keep this file up
to date in the same change whenever the architecture, modules, queries, data
model, or workflow change.

## What prowl is

A small terminal dashboard that watches a GitHub repo and re-renders on an
interval: **Open PRs → Merge Queue → Merged PRs → Commits**, with a reference
legend at the bottom. It rings the terminal bell when one of your PRs merges or
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
  and the screen clear are emitted by hand. Everything is gated on a `styled`
  flag (true only when stdout is a TTY) so piped/`--once` output is plain.
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
- `render.rs` — `Cell`/`Table`, width-aware padding (`unicode-width`), OSC-8,
  headers, reference legend, status line, loading screen, bell, clear.
- `queue.rs` / `prs.rs` / `merged.rs` — per-section rows, sorting, `to_table`.
- `commits.rs` — "commits by me" counts for the previous/next stable release
  (GitHub releases + compare REST APIs); best-effort, never fatal.
- `changes.rs` — `Tracker`/`Changes`: bell + highlight detection.
- `cache.rs` — per-repo on-disk cache of the last `Sections` under
  `$XDG_CACHE_HOME/prowl` (so the watch dashboard paints instantly on startup).
- `term.rs` — Unix terminal helper: while watching, quiet stdin (drop echo +
  line buffering, keep `ISIG` so signal keys work) and turn the interval wait
  into a poll, so `r` refreshes now while every other key is discarded; restored
  on every exit path. A no-op on non-Unix.
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
- **Cache:** on a watch start, prowl paints the cached `Sections` immediately
  (status line `cached HH:MM:SS · refreshing…`), seeds change-detection from it
  so the first live refresh highlights what changed while prowl wasn't running,
  but stays silent (no startup bell). `--no-cache` skips both read and write.
- **Terminal:** while watching, the cursor is hidden and stdin echo/line
  buffering are turned off, so stray keystrokes neither garble the dashboard nor
  spill into the shell; signal keys (Ctrl-C/Ctrl-Z) still fire. `r`/`R` forces a
  refresh now (shown with a dim `refreshing…` line). Both the cursor and terminal
  mode are restored on normal/`?` returns (Drop guards) and on SIGINT (the
  Ctrl-C handler).

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

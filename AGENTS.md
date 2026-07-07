# AGENTS.md

Orientation for AI agents (and humans) working on **prowl**. Keep this file up
to date in the same change whenever the architecture, modules, queries, data
model, or workflow change.

## What prowl is

A small terminal dashboard that watches a GitHub repo and re-renders on an
interval. It has two **views**, toggled with **Tab** (and chosen for one-shot
output with `--view`):

- **Mine** (default): **My open PRs → Merge Queue → My merged PRs → My
  Shipments**.
- **Reviews**: **Reviews** (open PRs awaiting / under my review, each with a
  per-row review-state glyph) **→ Reviewed & merged** (merged PRs I reviewed).

Below the active view is a `r refresh (every 5m) - tab switch view - ? help`
footer (which also shows the refresh interval) and an optional help legend last
at the bottom. While watching, the very top shows a `my PRs / reviews` tab strip
with the active view accented. It rings the terminal bell when one of your PRs
merges or an open PR's status changes, and flags the changed rows (the bell and
change markers track the Mine view only). It is a plain `std::thread::sleep`
redraw loop — **not** a raw-mode/alt-screen TUI — so output stays pipe-friendly
and URLs can be OSC-8 hyperlinks.

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

- `cli.rs` — clap derive CLI, `Section` enum, `View` (Mine/Reviews, `--view`,
  `.toggle()`), `ReviewScope` (Direct/All, `--review-scope`, `.qualifier()`),
  duration parser (`s/m/h/d/w`).
- `github.rs` — `Client` (HTTP `graphql()`/`get()`), `Repo`, `me()`,
  `default_branch()`, `detect_repo()` (parses the git `origin` remote),
  `parse_graphql()`.
- `auth.rs` — device-flow login + token storage (keyring/file).
- `model.rs` — serde structs + `fetch_*` for the queries; query strings. Covers
  the three Mine queries plus the Reviews view: `REVIEWS_QUERY` (one POST with
  two aliased searches, `requested:` + `reviewed:`) and `fetch_reviewed_merged`
  (reuses `merged_query`, now carrying `author`).
- `status.rs` — **the** palette: `Status`, `status_style`, glyphs/ASCII,
  `derive_status` (precedence), `fail_count`; the `mergeStateStatus` helpers
  `state_style`, `state_label` (DIRTY → CONFLICTS), `state_glyph`,
  `state_meaning`; and the Reviews-view `ReviewState` (Awaiting/ReReview/Updated/
  Reviewed) with `review_style`/`review_glyph`/`review_ascii`/`review_meaning`
  and `REVIEW_ORDER`.
- `render.rs` — `Cell`/`Table`, width-aware padding (`unicode-width`), OSC-8
  (incl. `link_styled` for clickable PR numbers), `truncate` + `fit_titles`
  (cap/align the shared `TITLE` column so every table lines up and the whole
  view stays within `MAX_WIDTH` = 120 columns), headers (`header`, with an
  optional dim count badge and trailing note — the queue ETA), the `tabs`
  view-switcher strip, the key-hint footer (`footer`, carrying the refresh
  interval), help
  legend (`help(view, …)` — contextual: status glyphs + every `STATE` value for
  Mine, review glyphs + the merged glyph for Reviews; last at the very bottom),
  loading screen, bell, clear.
- `queue.rs` / `prs.rs` / `merged.rs` — per-section rows, sorting, `to_table`.
  Each row's PR number is the OSC-8 link (no separate URL column); the queue
  columns are `# PR TITLE AUTHOR WAIT BUILD` (author truncated to
  `AUTHOR_WIDTH`), where `WAIT` is how long the entry has been queued (now −
  `enqueuedAt`) and `BUILD` is how long its speculative merge commit has been
  building — now − the earliest check-run `startedAt` in the commit's
  `statusCheckRollup.contexts` (`QueueEntryNode::build_started_at`), or `—` until
  a check actually starts running (still queued, or no speculative commit /
  checks). The rollup is a single flat connection (cheap, and front-loads the
  real check runs, unlike `checkSuites` whose first entries are app
  integrations). The `Merge Queue` header also carries the queue-level ETA
  (`~11m to merge`, from `mergeQueue.nextEntryEstimatedTimeToMerge`) as a dim
  note. The
  merged columns are `# PR TITLE RELEASE MERGED`, where `RELEASE` is the release
  that shipped the PR (a link to its release page) or `—` if not yet shipped,
  looked up from the `commits::ReleaseMap`.
- `reviews.rs` — the Reviews view's rows/tables. `ReviewRow` (open: `glyph PR
  TITLE AUTHOR UPDATED`, glyph = the `ReviewState`) via `build_open_rows`
  (de-dupes the two searches, derives the state, sorts by state rank then
  `updatedAt`) + `open_to_table`; `ReviewedMergedRow` (`glyph PR TITLE AUTHOR
  MERGED`) via `build_merged_rows` + `merged_to_table`.
- `commits.rs` — "commits by me" counts for the next (unreleased) version and
  the last 4 stable releases (GitHub releases + compare REST APIs); best-effort,
  never fatal. `fetch` returns both the `CommitStats` (rendered as the "My
  Shipments" section: one left-aligned labelled count per bucket, each label a
  link — `upcoming` to the compare log (last tag → default branch), each release
  tag to its release page, with each shipped release's relative publish age in a
  trailing dim column) and a `ReleaseMap` (PR number → the release that
  shipped it, parsed from each commit subject's trailing `(#NNN)`, the squash /
  merge-commit convention) that annotates the merged section's `RELEASE` column.
  `--include-pre-releases` also counts prereleases (drafts are always skipped).
- `changes.rs` — `Tracker`/`Changes`: bell + highlight detection (Mine view).
- `cache.rs` — per-repo on-disk cache of the last `Sections` under
  `$XDG_CACHE_HOME/prowl` (so the watch dashboard paints instantly on startup).
- `term.rs` — Unix terminal helper: while watching, quiet stdin (drop echo +
  line buffering, keep `ISIG` so signal keys work) and turn the interval wait
  into a poll, so `r` refreshes now, `Tab` switches view, and `?` toggles the
  help legend, while every other key is discarded; restored on every exit path.
  A no-op on non-Unix.
- `timefmt.rs` — `chrono` helpers (local clock, `mergedAt` ages, since-date).

## Key behaviors

- **Status precedence:** `merged > conflicts > fail > pending > pass > none`.
  Check suites with **zero check runs** (`checkRuns.totalCount == 0`) are
  phantom and ignored for both the glyph and the `FAIL` count, matching GitHub's
  rollup (so a `CLEAN` PR stays green).
- **Sorting:** open PRs by `updatedAt` desc, merged PRs by `mergedAt` desc;
  queue by `position` asc. Reviews by review-state rank (Awaiting → ReReview →
  Updated → Reviewed) then `updatedAt` desc; reviewed-and-merged by `mergedAt` desc.
- **Views / Tab:** two views, `Mine` (default) and `Reviews`, selected for
  one-shot output with `--view` and toggled live with `Tab`. While watching,
  prowl fetches **both** views every refresh so Tab switches instantly from
  `last_good` (no refetch); `--once`/piped fetches only the selected view. A
  top tab strip marks the active view.
- **Review state:** each open review row is `Awaiting` (requested, not yet
  reviewed by me), `ReReview` (requested again after I reviewed), `Updated` (I
  reviewed; last commit `committedDate` > my latest review `submittedAt`), or
  `Reviewed`. `--review-scope` picks the requested search: `all` →
  `review-requested:<me>` (me + my teams, default), `direct` →
  `user-review-requested:<me>` (only me). Both review searches exclude my own
  PRs (`-author:<me>`).
- **Bell:** rings once per refresh when a PR of mine merges or an open PR's
  status changes (keyed by PR number, so re-sorting / new PRs / title edits do
  not ring). The first refresh is silent. Changed rows get a `▸` marker. Bell
  and change markers track the **Mine** view only (the Reviews view conveys
  state through its per-row glyph instead).
- **Resilience:** a failed API call keeps the last good data, shows a dim error
  line, and does not ring.
- **Cache:** on a watch start, prowl paints the cached `Sections` immediately,
  seeds change-detection from it
  so the first live refresh highlights what changed while prowl wasn't running,
  but stays silent (no startup bell). `--no-cache` skips both read and write.
- **Terminal:** while watching, the cursor is hidden and stdin echo/line
  buffering are turned off, so stray keystrokes neither garble the dashboard nor
  spill into the shell; signal keys (Ctrl-C/Ctrl-Z) still fire. `r`/`R` forces a
  refresh now; `Tab` switches view; `?` toggles the help legend
  (contextual to the active view — status glyphs + `STATE` values for Mine,
  review glyphs for Reviews — hidden by default, rendered last at the very
  bottom; `--no-help` only affects one-shot/piped output). The only persistent
  bottom line is the footer
  (`r refresh (every 5m) - tab switch view - ? help`), which carries the refresh
  interval; a failed refresh adds a dim `error: …` line above it. The blocking
  fetch runs on a worker thread (`std::thread::scope`) while the main thread
  keeps polling input, so `?` and `Tab` stay responsive even mid-refresh. Both
  the cursor and terminal mode are
  restored on every normal or early (`?`-operator) return (Drop guards) and on
  SIGINT (the Ctrl-C handler).

## The GraphQL queries + REST (see `model.rs` / `commits.rs`)

- Merge queue: `repository.mergeQueue.entries` (vars `owner`, `name`), each
  entry carrying `enqueuedAt` (WAIT) and `headCommit.statusCheckRollup.contexts`
  check-run `startedAt` timestamps (BUILD = now − the earliest), plus the
  queue-level `nextEntryEstimatedTimeToMerge` (the header ETA).
- Open PRs: `search(is:pr is:open author:<me>)` with `mergeable`,
  `mergeStateStatus`, `mergeQueueEntry`, last commit `checkSuites { conclusion
  checkRuns { totalCount } }`, `updatedAt`.
- Merged: `search(is:pr is:merged author:<me> merged:>=<since>)` with `mergedAt`
  (fetched `sort:updated-desc`, since search can't sort by merge time, then
  re-sorted by `mergedAt` for display). Now also fetches `author` (used by the
  reviewed-and-merged section; the Mine merged section ignores it).
- Reviews (one POST, two aliased searches): `requested: search(is:pr is:open
  <scope>:<me> -author:<me>)` and `reviewed: search(is:pr is:open
  reviewed-by:<me> -author:<me>)`, each node carrying `author`, last commit
  `committedDate`, and `reviews(author:<me>)` `submittedAt`s. Re-review = a PR
  in both result sets.
- Reviewed & merged: `search(is:pr is:merged reviewed-by:<me> -author:<me>
  merged:>=<since>)` (reuses the merged query/limit).
- Commits section: REST `GET /repos/.../releases`, `/compare/a...b`, `/commits`.

## Build / test / lint

```sh
cargo build                                  # must be warning-free
cargo clippy --all-targets -- -D warnings    # must be clean
cargo fmt --all --check                      # must be formatted
cargo test                                   # offline, fixture-based
```

`lib.rs` opts the crate into `#![warn(clippy::pedantic)]` with a curated block of
`#![allow(...)]`s (each justified) for the lints that are noise for a small
bin-plus-test-lib — so `clippy -D warnings` still runs pedantic and new pedantic
findings fail CI.

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

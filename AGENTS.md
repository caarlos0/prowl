# AGENTS.md

Orientation for AI agents (and humans) working on **prowl**. Keep this file up
to date in the same change whenever the architecture, modules, queries, data
model, or workflow change.

## What prowl is

A small terminal dashboard that watches a GitHub repo and re-renders on an
interval. It has two **views**, toggled with **Tab** (and chosen for one-shot
output with `--view`):

- **Mine** (default): **My open PRs → Merge Queue → My merged PRs → My
  Shipments** (headers accented green / peach / mauve / blue respectively).
- **Reviews**: **Reviews** (open PRs awaiting / under my review, each with a
  per-row review-state glyph) **→ Reviewed & merged** (merged PRs I reviewed).

Below the active view is an optional help legend, an optional search prompt, and
last a `r refresh (every 5m) - tab switch view - enter open -
y copy - / search - ? help` footer (which also shows the refresh interval, and
reads `r refreshing` while a fetch is in flight). While watching, the very top
shows a `my PRs / reviews` tab strip with the active view accented. It rings the
terminal bell when one of your PRs merges or an open PR's status changes, and
flags the changed rows (the bell and change markers track the Mine view only).
The interactive watch runs on the
[**uncurses**](https://github.com/aymanbagabas/uncurses) toolkit with an event
loop: it shows an *inline* `Loading...` frame, then enters the **alternate
screen** once the first fetch lands (or immediately when there's a
cache to paint), where that bottom block is **pinned** to the last rows and the
body adapts to the available height. Interactive `--once` uses an *inline* surface instead:
a `Loading...` frame while the fetch runs (abortable with `q`), then the dashboard
is left in the terminal. Piped/non-TTY output is plain text printed straight to
stdout, so the dashboard stays pipe-friendly and URLs can be OSC-8 hyperlinks.

## Golden rules

- **Transport is the native GitHub API over HTTP** (`ureq` + rustls), not the
  `gh` CLI. `github::Client` sends a Bearer token with a User-Agent +
  `X-GitHub-Api-Version`. GraphQL is a `POST /graphql` with `{query, variables}`;
  parse the full `{"data":...}` envelope (`github::parse_graphql`, surfacing
  GraphQL `errors`). REST is `GET /<path>`.
- **Auth** lives in `auth.rs`: token resolution is `PROWL_TOKEN` → `GITHUB_TOKEN`
  → OS keyring / chmod-600 file → OAuth **device flow** (interactive). The OAuth
  App client id is public and embedded. `--login` forces the device flow. The
  device prompt is written to stderr, so its link is styled from stderr's own
  TTY/profile detection and stays plain when stderr is redirected.
- **The terminal toolkit is `uncurses`** (the author's own low-level library):
  its `style::Style` carries SGR + the OSC-8 link, `Program` owns raw mode / the
  alternate screen / input / teardown while `Screen` is the renderer it draws
  through (`program.screen_mut()`), and `text` provides width math.
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

- `cli.rs` — clap derive CLI, `Section` enum, `View` (Mine/Reviews, `--view`,
  `.toggle()`), `ReviewScope` (Direct/All, `--review-scope`, `.qualifier()`),
  `--required` for required-only CI counts,
  duration parser (`s/m/h/d/w`), and the `WATCH_KEYS` `after_help` block
  documenting the interactive watch-mode keys.
- `github.rs` — `Client` (HTTP `graphql()`/`get()`), `Repo`, `me()`,
  `default_branch()`, `detect_repo()` (parses the git `origin` remote),
  `parse_graphql()`.
- `auth.rs` — device-flow login + token storage (keyring/file).
- `model.rs` — serde structs + `fetch_*` for the queries; query strings. Covers
  the three Mine queries plus the Reviews view: `REVIEWS_QUERY` (one POST with
  two aliased searches, `requested:` + `reviewed:`) and `fetch_reviewed_merged`
  (reuses `merged_query`, now carrying `author`).
- `status.rs` — **the** palette: `Approval` (Approved/Pending) with
  `approval_of` (any `latestOpinionatedReviews` state is `APPROVED`),
  `approval_style`/`approval_glyph`/`approval_ascii`/`approval_meaning` and
  `APPROVAL_ORDER`; the conflict marker — `conflicts_of` (true when `mergeable`
  or `mergeStateStatus` reports a conflict; every other reason a merge waits has
  its own column), `conflict_marker` and `CONFLICT_MEANING`; the check semaphore — `Lamp` (Fail/Running/Pass),
  `lamp_color`, `Checks` (fail/running/pass counts) and the state→lamp maps
  `check_run_lamp` / `status_context_lamp`; `Status` + `derive_status`, which is
  now only the bell's coarse change key (nothing renders it); and the
  Reviews-view `ReviewState` (Awaiting/ReReview/Updated/Reviewed) with
  `review_style`/`review_glyph`/`review_ascii`/`review_meaning` and
  `REVIEW_ORDER`. Colors are `uncurses::color::Color` constants and `fg(Color)`
  builds the foreground `Style`.
- `render.rs` — the surface painters: `paint_table`/`paint_header`/`paint_dim`/
  `paint_footer`/`paint_tabs`/`paint_search_prompt`/`paint_help` write onto any
  `&mut impl TextSurface` using the surface's own `str_width` (no in-house width
  math) and `set_str` (column gaps are implicit — unpainted cells stay blank, so
  no padding is emitted). `Cell` (text + `Style`, the OSC-8 link folded into the
  style) / `Table`, `truncate` (uncurses' width-aware truncator), and the
  responsive table layout. Tables fill the live surface width; `TITLE` is the
  largest flexible column and optional `BRANCH` is second. As width falls,
  columns right of `TITLE` disappear from right to left, with `BRANCH` removed
  last; `FAIL`/`RUN`/`PASS` hide as one semantic group, never as partial lamps.
  `TableAlignment` shares the two-column gutter (marker, glyph) and PR widths
  across all
  tables in a view; when branches are shown it also shares TITLE width, so PR,
  TITLE, and BRANCH all start on the same columns. Below 24 columns the
  dashboard reports `Terminal too small — need W×H.` Piped output and screenshots use
  `OUTPUT_WIDTH` = 120 because they have no live screen dimensions. Headers (with an optional dim
  count badge and trailing note — the queue ETA), the `tabs` view-switcher strip,
  the leading-column `change_marker` and the selected-row highlight
  (`highlight_row` paints the selection background edge to edge across one
  screen row, once the body is painted — so it covers the
  hand-laid-out shipments section too, and the change marker stays visible
  underneath instead of being overwritten by a caret), the key-hint footer
  (carrying the refresh interval and the `enter open` / `/ search` hints, plus
  `+ resize for more` when width or height hides information), the search prompt
  line (the `/` query + match count; it paints no cursor and instead *returns*
  the caret cell, so the watch can park the terminal's real one there), and the
  help legend
  (`paint_help(view, …)` — a movement-keys line then, contextual: the
  approval glyphs and the conflict marker for Mine, review glyphs for Reviews; the column headers
  speak for themselves and are not repeated; first in the bottom block, above the
  search prompt and footer) live here too, plus `render_table`
  (paint one table to a string, for tests) and `paint_dim`/`paint_dim_at`, the
  dim one-liners — the trailing status line flush left, an empty section's
  placeholder indented by `ROW_INDENT` so it lines up with the rows it stands in
  for.
  It also owns the watch frame's geometry: `compose(screen, body, bottom, rows,
  caret)` fills exactly `rows` rows — as much of the body as fits at the top,
  blank padding, then the bottom block glued to the last rows — and returns the
  row that block starts on. Before composition, `responsive_layout` hides help,
  then Shipments; progressively trims Merged to the newest row; narrows Queue
  to building + own rows, then building rows; and finally hides each section.
  Each partial section ends with `+N hidden`. The Reviews view still hides
  Reviewed & merged as one section. Navigation, search counts, open, and copy
  use the same `Visibility`, including its row limits. If the protected open-PR
  section does not fit whole, the frame is replaced by
  `Terminal too small — need W×H.`
  The body is drawn through
  a `uncurses::buffer::View`, which clips without translating, so blitting it maps
  the first visible body row onto the top of the screen.
- `queue.rs` / `prs.rs` / `merged.rs` — per-section rows, sorting, `to_table`.
  Each row's PR number is the OSC-8 link (no separate URL column). The open-PRs
  columns are `[mark] [A] PR TITLE [BRANCH] THREADS FAIL RUN PASS`: `A` is the
  approval glyph, a conflicting PR prefixes its own `TITLE` with the red conflict
  marker (`status::conflict_marker`) instead of spending a column every other row
  would leave blank, `FAIL`/`RUN`/`PASS` are the check-run semaphore
  (always all three, dim when zero, colored when not) and `THREADS` the
  unresolved review threads (`100+` when the page was capped).
  `--branch` adds `BRANCH` to every PR table; `prs::without_drafts` backs
  `--no-draft`. The queue
  columns are `# [blank] PR TITLE [BRANCH] AUTHOR WAIT BUILD FAIL RUN PASS` (author truncated to
  `AUTHOR_WIDTH`), where `WAIT` is how long the entry has been queued (now −
  `enqueuedAt`) and `BUILD` is how long its speculative merge commit has been
  building — now − the earliest check-run `startedAt` in the commit's
  `statusCheckRollup.contexts` (`QueueEntryNode::build_started_at`), or `—` until
  a check actually starts running (still queued, or no speculative commit /
  checks). `FAIL`/`RUN`/`PASS` is the same check semaphore as the open-PRs table
  (`render::lamp_cell`), counting the speculative merge commit's checks from the
  rollup's own `checkRunCountsByState` / `statusContextCountsByState` aggregates
  (`QueueEntryNode::checks`). The rollup is a single flat connection (cheap, and
  front-loads the real check runs, unlike `checkSuites` whose first entries are
  app integrations). The `Merge Queue` header also carries the queue-level ETA
  (`~11m to merge`, from `mergeQueue.nextEntryEstimatedTimeToMerge`) as a dim
  note. The
  merged columns are `[mark] [blank] PR TITLE [BRANCH] RELEASE MERGED` (no
  per-row glyph — every row there is merged — so a blank gutter cell keeps its
  left columns aligned with every other PR table), where `RELEASE` is the release
  that shipped the PR (a link to its release page) or `—` if not yet shipped,
  looked up from the `commits::ReleaseMap`.
- `reviews.rs` — the Reviews view's rows/tables. `ReviewRow` (open: `glyph PR
  TITLE [BRANCH] AUTHOR UPDATED`, glyph = the `ReviewState`) via `build_open_rows`
  (de-dupes the two searches, derives the state, sorts by state rank then
  `updatedAt`) + `open_to_table`; `ReviewedMergedRow` (`glyph PR TITLE [BRANCH]
  AUTHOR MERGED`) via `build_merged_rows` + `merged_to_table`.
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
- `nav.rs` — watch-mode row navigation + search: `groups(view, &Sections, query)`
  is the matching rows' open URLs bucketed by rendered section, `targets` is that
  flattened (PR rows → the PR; shipments → the release / compare log; url-less
  rows skipped) so a selection index lines up with the rendered rows,
  `section_at(…, index)` is the one group holding `index` (what `Y` copies; an
  empty section holds no index, so index 0 means the first non-empty one),
  `filter(&Sections, query)` clones the matching rows for rendering
  (same per-row haystack — number/title/author/tag — so rows and targets stay in
  lockstep), `moved` advances the selection cursor by a `nav::Move` (the
  input-agnostic movement type — `lib.rs::classify` maps keys onto it; lazy:
  `None` until the first move, `Bottom` enters at the last row). Refreshes and
  resizes restore the same selected URL when it remains visible.
- `open.rs` — `open::url` opens a URL in the default browser via the platform
  opener (`open` / `xdg-open` / `cmd /C start`), spawned detached; rejects
  non-`http(s)` URLs; no new dep.
- `clipboard.rs` — `clipboard::copy` sets the clipboard with the OSC 52 escape
  (`ESC ] 52 ; c ; <base64> BEL`) written straight to stdout, plus the ~15-line
  base64 encoder it needs. No dep, no subprocess, and it reaches the clipboard of
  the terminal you're *looking at*, so it works over SSH; the terminal has to
  support it (tmux needs `set -g set-clipboard on`) and silently ignores it
  otherwise, hence the "copied N links" wording.
- `cache.rs` — per-repo on-disk cache of the last `Sections` under
  `$XDG_CACHE_HOME/prowl` (so the watch dashboard paints instantly on startup).
- `timefmt.rs` — `chrono` helpers (local clock, `mergedAt` ages, since-date).

`run()` first creates a `uncurses::terminal::Terminal::stdio()`; interactivity is
its `is_terminal().1` (output a TTY?). When the output is **not** a TTY (piped,
redirected), `render_once` paints the dashboard onto an offscreen `TextBuffer`
sized to its content (a generous `height_bound` + `bottom_bound`, then cropped to
the painted height), and `encode_with`s it to the terminal's output (`Terminal::output`)
using the **detected** color `Profile` (`Profile::detect_from`), so it's colored on
a TTY and plain when piped. Interactive `--once` instead runs `run_once_interactive`:
an *inline* `Program` (raw mode, hidden cursor) shows a `Loading...` frame while the
fetch runs on a background thread, so keystrokes don't echo and `q`/`Esc`/`Ctrl-C`
aborts mid-fetch; on success the dashboard replaces the frame and is left inline
(`Program::finish` doesn't wipe an inline surface). Otherwise the same `Terminal` is
moved into `App::start` → `Program::new(terminal)`. The watch redraw and the inline
one-shot frame share `render_dashboard`, which has two layouts: **pinned** (the
watch, in the alternate screen) fills the terminal, scrolls the body under a
bottom block glued to the last rows, and **unpinned** (the inline one-shot) sizes
the surface to the content and crops to the painted height.

The interactive watch is `lib.rs::App`, following the uncurses example **`App`
pattern**: the struct owns the `uncurses::Program` plus all dashboard state, and
`run()` does `let mut app = App::start(terminal, ...)?; let result = app.run();
app.stop()?; result`. `start` builds the screen from the `Terminal` and brings it
up (raw mode, hidden cursor, keeping the terminal's detected color profile), then
paints the startup frame: a cached dashboard if one exists (entering the alt screen
straight away), otherwise an **inline** `Loading...` frame. `run` resolves
`me`/default branch then loops fetch → paint → wait, returning `Ok(())` on a quit
key. The first live paint calls `enter_alt` (once), which drops the inline frame to
zero rows and switches to the alt screen — so loading looks like ordinary command
output before the dashboard takes over the screen. `stop` consumes the app and calls
**`Program::finish`** (the idiomatic teardown: exit alt-screen, show cursor, leave
raw mode). Because the caller always runs `stop`, the terminal is restored on
every path — a clean quit, a `?`-operator error, or a failed first paint (`start`
calls `stop` itself before bailing). Each frame is painted by `redraw` →
`render_dashboard`, which pins the frame once `in_alt` is set: `paint_body` and
`paint_bottom` each paint into their own `TextBuffer`, and `render::compose`
places them. The renderer's line-scroll capabilities (CSR/SU-SD/IL-DL) need no
stripping: uncurses gates scroll detection on `scroll_optimize && sync_output &&
fullscreen`, so a scroll — which moves full rows, including the pinned bottom
block — is only ever emitted inside a frame the terminal confirmed it presents
atomically. The managed area is **never** refitted per frame — that would
re-query the terminal on every redraw (and, before uncurses 0.0.2, forced a
clear + full repaint: the screen erased and rewrote itself every frame instead
of the renderer emitting a diff, which is what flicker looks like). It is sized
in exactly two places. `autoresize` is called once, in `enter_alt`: it is the
only call that queries the terminal for its row count, which is needed right
after the inline frame is collapsed to zero rows, and it fits the area to the
whole window — correct precisely because we just took the alt screen. A resize
event instead carries its own dimensions, so `Action::Resize` / `SearchAction::
Resize` call `Screen::resize` with them and skip the query; `Screen::resize`
re-establishes the area whatever the size, so even a same-cell-size resize
fully clears stale column positions; inline (the loading frame, before `in_alt`)
the reported height is the *window's*, so the managed area keeps its own height
and follows only the width. `Program::init` does not
probe the terminal, so `start` calls **`query_capabilities`**: reading an event
records the reply as it passes through, and the DECRPM 2026 answer is what
enables **synchronized output**, so a frame that clears first (a resize) is
presented atomically rather than seen half-drawn. The same query also adopts
grapheme clustering and in-band resize where the terminal supports them. The
loop uses `poll_event` with
the interval as the timeout. Keys are classified into an `Action` (or, while the
search prompt is open, a `SearchAction`) with `Key::matches`, which is
**case-sensitive** — bindings must list both cases (`["r", "R"]`). `r`/`R`
refresh now, `Tab` switches view, `?` toggles help, `/` opens search, `Enter`
opens the selected row, `y`/`Y` copy links, the movement keys drive the cursor,
`q`/`Q`/`Ctrl-C` quit (`Esc` clears the filter, or quits when there is none),
`Ctrl-Z` suspends/resumes, `Resize` repaints. All watch UI state lives in one
`Ui` struct (view, help, selection, search, `--branch`). `ctrlc` handles external
SIGINT/SIGTERM/SIGHUP by asking the event loop to stop; the owning `Program`
still performs all teardown through `finish`.

## Key behaviors

- **Approval glyph:** approved when any reviewer's latest opinionated review is
  `APPROVED`, and nothing else feeds it — a later change request does not undo
  it, because `THREADS` already reports what is still open. GitHub's own
  `reviewDecision` is deliberately unused: it is null wherever no branch rule
  requires a review, and it answers "may this merge?", not "did anyone
  approve?". Across 189 real PRs it never reported `APPROVED` without an
  approving review, so it adds nothing here.
- **Conflict marker:** a conflicting PR (`mergeable: CONFLICTING` or
  `mergeStateStatus: DIRTY` — the two are computed by the same job and one can
  land first) prefixes its title with a red marker; nothing else marks the
  title. Being blocked on reviews, required checks, a stale base, or draft
  status is the approval glyph's, the semaphore's, or the dimmed PR number's
  job.
- **Check counts** come from the rollup's `checkRunCountsByState` /
  `statusContextCountsByState` aggregates, so they're exact and unpaginated —
  no phantom zero-run check suites and no truncated page to compensate for.
  With `--required`, prowl follows those contexts page by page in a second
  batched GraphQL query and uses each context's `isRequired(pullRequestNumber:)`
  value for both the semaphore and the merge queue's BUILD start.
  `STALE` lights no lamp (it neither blocks nor runs).
- **Status precedence** (the bell key only): `conflicts > fail > running > pass
  > none`.
- **Sorting:** open PRs by `updatedAt` desc, merged PRs by `mergedAt` desc;
  queue by `position` asc. Reviews by review-state rank (Awaiting → ReReview →
  Updated → Reviewed) then `updatedAt` desc; reviewed-and-merged by `mergedAt` desc.
- **Queue dedup:** a PR of mine that's in the merge queue is shown only in the
  Merge Queue section, not the open-PRs list (`prs::without_queued`, applied at
  layout time only while the queue section is visible, so `--only mine` and
  height-driven queue hiding still list it).
- **Drafts:** `--no-draft` hides draft PRs from both the Mine open-PRs list and
  the Reviews list (`prs::without_drafts` / `reviews::without_drafts`).
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
- **Navigation / open:** a lazy selection cursor (`nav`, watch only) — `None`
  until the first movement key, then the chosen row is painted with the
  selection background (`status::SURFACE`) edge to edge across the full width
  (`render::highlight_row`, applied once the body is painted) — no caret glyph,
  so the change marker still shows through, and the custom shipments painter is
  covered for free. `j`/`k` (or the arrows) move one row, `g`/`G` jump to
  first/last,
  `Ctrl-D`/`Ctrl-U` half a page (sized from the screen's `window_cells`); Enter
  opens the selected row — the PR, or a shipments release / the upcoming compare
  log — via `open::url`. Resize preserves the selected URL when it remains
  visible and clears selection when responsive hiding removes it. Every row
  across all sections of the active view is one
  target (`nav::targets_visible`, in render order); switching views drops the cursor and
  a refresh preserves its URL when that row remains visible. `--once`/piped
  output has no selection.
- **Copy:** `y` copies the selected row's link, `Y` every link of the section the
  cursor is in (`nav::section_at_visible`) as a markdown list (`- <url>` per line, no
  trailing newline). Both honor the active search filter, so `Y` copies only the
  visible matches; with no cursor yet `Y` takes the first non-empty section. The
  outcome ("copied N links", or a `copy failed:` error) lands on the same dim
  trailing line as a refresh error and is cleared by the next refresh. Watch mode
  only — `--once`/piped output has no keys.
- **Search / filter:** `/` opens a search prompt (`Ui.searching`); typing filters
  the rows live (case-insensitive substring over number/title/author/release
  tag), Enter applies the filter and returns to the list, Esc (or a lone Esc from
  the list) clears it — and with no filter to clear, Esc quits. While the prompt
  is open every keystroke is text (`classify_search`), else keys are normal-mode
  actions (`classify`). `nav::filter` produces the rendered rows and
  `nav::targets_visible(…, query)` the navigable ones from the **same** predicate, so the
  caret/open track the visible matches; the selection resets on each edit. The
  prompt uses the **terminal's own cursor**: `paint_search_prompt` returns the
  caret cell, `paint_dashboard` passes it up only while `searching`, and
  `render_dashboard` stages it with `Screen::set_cursor_position` (declarative —
  `render` re-applies it every frame) or `clear_cursor_position`. `App` tracks
  `cursor_shown` and only calls `show_cursor`/`hide_cursor` on a transition,
  since both always emit DECTCEM. Watch mode only.
- **Cache:** on a watch start, prowl paints the cached `Sections` immediately
  (entering the alt screen straight away), seeds change-detection from it
  so the first live refresh highlights what changed while prowl wasn't running,
  but stays silent (no startup bell). With no cache it shows an inline
  `Loading...` frame and enters the alt screen only once the first fetch lands.
  `--no-cache` skips both read and write.
- **Terminal:** the watch runs on a `uncurses::Program` in the alternate screen
  with the cursor hidden (it reappears only in the search prompt); raw mode means stray keystrokes never garble the
  dashboard or spill into the shell. `r`/`R` forces a refresh now; `Tab` switches
  view; `?` toggles the help legend (contextual to the active view —
  approval glyphs and the conflict marker for Mine, review glyphs for Reviews — hidden by
  default, rendered at the top of the bottom block, above the search prompt and
  footer whose keys it documents; `--no-help` only affects
  one-shot/piped output). The movement keys (`j`/`k`, arrows, `g`/`G`,
  `Ctrl-D`/`Ctrl-U`) drive the selection cursor, Enter opens it, `y`/`Y` copy it
  (row / whole section), and `/` filters.
  `q`/`Q`/`Ctrl-C` quit (as does `Esc` with no filter applied) and `Ctrl-Z`
  suspends/resumes. The bottom block — help legend, search prompt, error line,
  footer — is **pinned** to the last rows of the screen (`render::compose`).
  Height pressure hides help and Shipments, trims Merged oldest-first to one
  row, then narrows Queue to building + own rows and building-only before hiding
  it. Partial sections show `+N hidden`. The Reviews view hides Reviewed & merged
  as one section. Open PRs remain whole; if they cannot fit, the frame says
  `Terminal too small — need W×H.` The only persistent
  bottom line is the footer
  (`r refresh (every 5m) - tab switch view - enter open - y copy - / search - ?
  help`), which carries the refresh interval and progressively removes
  low-priority labels/hints to fit narrow widths instead of clipping; a failed refresh adds a dim
  `error: …` line above it (the same slot a copy's `copied N links`
  confirmation uses). While a fetch is in flight the footer reads `r refreshing` with the
  `r` glyph dimmed. Every fetch (and the one-time `me`/default-branch resolution)
  runs on a **detached background thread** and returns over a channel; the main
  thread only polls input and paints, so network I/O never blocks the UI —
  navigation, search, `Tab`, `?`, resize and suspend stay live mid-refresh and
  **quit is instant** (a quit abandons the in-flight request, which is reaped at
  process exit). The terminal is restored on every exit path by `App::stop`
  (`Program::finish`), which the caller always runs after `App::run`; external
  SIGINT/SIGTERM/SIGHUP use the same cooperative path. Pinned staging buffers
  inherit the live screen's grapheme and East Asian width policy.
- **Interactive `--once`:** `run_once_interactive` brings up an *inline* `Program`
  (raw mode, hidden cursor) and paints a `Loading...` frame while the fetch runs on
  a background thread, so keystrokes don't echo and `q`/`Esc`/`Ctrl-C` aborts the
  fetch instantly. On success the dashboard replaces the frame and is left inline in
  the terminal; on abort the frame is wiped. `Program::finish` restores the terminal
  on every path. Piped/non-TTY output keeps the plain `render_once` encode path.

## The GraphQL queries + REST (see `model.rs` / `commits.rs`)

- Merge queue: `repository.mergeQueue.entries` (vars `owner`, `name`), each
  entry carrying `headRefName`, `enqueuedAt` (WAIT) and `headCommit.statusCheckRollup.contexts`
  check-run `startedAt` timestamps (BUILD = now − the earliest) plus that same
  connection's `checkRunCountsByState` / `statusContextCountsByState` aggregates
  (the FAIL/RUN/PASS semaphore), plus the queue-level
  `nextEntryEstimatedTimeToMerge` (the header ETA).
- Open PRs: `search(is:pr is:open author:<me>)` with `mergeable`,
  `mergeStateStatus`, `latestOpinionatedReviews(first: 100) { nodes { state } }`,
  `mergeQueueEntry`, `headRefName`, `updatedAt`,
  `reviewThreads(first: 100) { totalCount nodes { isResolved } }` (no unresolved
  aggregate exists, hence the page + a `+` when capped), and the last commit's
  `statusCheckRollup { contexts(first: 1) { checkRunCountsByState
  statusContextCountsByState } }` — the aggregates only, no context nodes.
- Merged: `search(is:pr is:merged author:<me> merged:>=<since>)` with
  `headRefName` and `mergedAt`
  (fetched `sort:updated-desc`, since search can't sort by merge time, then
  re-sorted by `mergedAt` for display). Now also fetches `author` (used by the
  reviewed-and-merged section; the Mine merged section ignores it).
- Reviews (one POST, two aliased searches): `requested: search(is:pr is:open
  <scope>:<me> -author:<me>)` and `reviewed: search(is:pr is:open
  reviewed-by:<me> -author:<me>)`, each node carrying `author`, `headRefName`,
  last commit `committedDate`, and `reviews(author:<me>)` `submittedAt`s.
  Re-review = a PR
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

CI (`.github/workflows/build.yml`) runs fmt/clippy/build/test (the `build` job)
and `cargo audit` for dependency advisories (the `audit` job) on push and PRs.

## The README screenshot

`task screenshot` regenerates it. `examples/demo.rs` builds a fake `Sections`
(made-up repo, PRs and authors; timestamps relative to now) and prints it
through the real `render_to_string` — the same painters `--once` uses — so the
shot can't drift from the layout, which is why `Sections`, `Ui` and
`render_to_string` are
`pub`, same as everything else the offline tests reach. `demo.tape` shoots it
with [vhs](https://github.com/charmbracelet/vhs) (Nerd Font, Catppuccin Mocha;
the trailing `Sleep` is required or vhs exits before writing the file), and the
task uploads `demo.png` to GitHub's CDN and rewrites the `<img>` URL in
`README.md`. `demo.png` is gitignored — no binary in the repo. An uploaded asset
only goes public once its URL appears in a comment/issue/PR body (a commit that
references it does *not* count), so the task posts and deletes a commit comment,
then polls the URL anonymously until it answers 200.

## Releases

`task release` cuts one: `svu next` picks the version, writes it to
`Cargo.toml`, refreshes `Cargo.lock`, commits (`chore(release): vX.Y.Z`), tags,
pushes, and watches the workflow run. It only runs from a clean `main`.

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

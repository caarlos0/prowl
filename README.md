# prowl

Watch a GitHub repository's **open PRs**, its **merge queue**, and your
**recently merged PRs** as colored, aligned tables. prowl refreshes on an
interval and **rings the terminal bell** when a PR of yours merges or an open
PR's status changes — so you can leave it running in a split and get a hoot the
moment something needs you. The rows that changed are flagged on the next
refresh.

It replaces the `while true; clear; figlet ...; <cmd>; sleep 10m; end` loops you
otherwise babysit.

```
Open PRs (3)
      PR     TITLE                                        STATE    QUEUE  FAIL  URL
   P  #6656  feat: goreleaser check --allow-deprecations  BLOCKED  -      -     https://github.com/goreleaser/goreleaser/pull/6656
 ▸ !  #6475  feat: install scripts                        DIRTY    -      3     https://github.com/goreleaser/goreleaser/pull/6475
   !  #5323  feat: make .Artifacts ... template names     DIRTY    -      -     https://github.com/goreleaser/goreleaser/pull/5323

No merge queue (or it is empty) for goreleaser/goreleaser.

Merged PRs (2)
      PR     TITLE                            BASE  MERGED  URL
   m  #6649  fix(mcp): clean subfolder path    main  1w      https://github.com/goreleaser/goreleaser/pull/6649
   m  #6634  feat(dockers_v2): pre/post hooks  main  3w      https://github.com/goreleaser/goreleaser/pull/6634

Reference
  P pass — all checks that ran passed
  ! conflicts — merge conflict; needs a rebase
  m merged — merged
  BLOCKED — blocked; required reviews or checks not satisfied
  DIRTY — merge conflict

updated 10:15:05 — changed · next 10:20:05
```

The above is the plain output you get when piping or with `--once`. In a real
terminal each non-empty section gets a bold colored `▌` header bar, the status
column is a single colored Nerd Font glyph (`P`/`!`/`m` become real icons), the
`▸` flags rows that changed since the last refresh, the `URL` column is a dim,
underlined, clickable OSC-8 hyperlink, and everything uses the Catppuccin Mocha
palette. Sections render top-to-bottom as **Open PRs → Merge Queue → Merged
PRs**; open and merged PRs are sorted by last update time.

## How it works

prowl shells out to the [`gh`](https://cli.github.com) CLI for everything — no
tokens, no HTTP client. `gh` handles authentication, and it matches the scripts
this tool replaces. Each refresh runs a few `gh api graphql` calls, parses the
JSON, and redraws. A failed call (network blip, rate limit) never crashes the
loop and never rings the bell: prowl shows a dim error line, keeps the last good
data, and retries next tick.

## Requirements

- [`gh`](https://cli.github.com) authenticated (`gh auth login`).
- A terminal with a [Nerd Font](https://www.nerdfonts.com/) for the status
  glyphs (use `--ascii` otherwise).
- 24-bit truecolor terminal for the palette.

## Install

```sh
cargo install --path .
# or
cargo build --release   # -> target/release/prowl
```

Desktop notifications (`--notify`) are behind an optional feature:

```sh
cargo install --path . --features notify
```

## Usage

```
prowl [--repo <owner/name>] [--interval <dur>] [--once] [--no-bell] [--ascii]
      [--only queue,mine,merged] [--merged-window <dur>] [--merged-limit <n>]
      [--no-reference] [--notify]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--repo <owner/name>` | auto-detect | Repository to watch. |
| `--interval <dur>` | `5m` | Refresh cadence (`30s`, `10m`, `2h`, ...). |
| `--once` | | Render a single frame and exit (no loop, no bell). |
| `--no-bell` | | Never ring the bell on changes. |
| `--ascii` | | Use ASCII status letters instead of Nerd Font glyphs. |
| `--only <list>` | all | Comma-separated subset of `queue,mine,merged`. |
| `--merged-window <dur>` | `2d` | How far back "recently merged" reaches (`7d`, `48h`, `2w`). |
| `--merged-limit <n>` | `20` | Max recently-merged PRs to list. |
| `--no-reference` | | Hide the reference legend at the bottom. |
| `--notify` | | Also send a desktop notification (needs the `notify` feature). |

Durations accept `s`, `m`, `h`, `d`, `w` (a bare number is seconds).

When stdout is **not** a TTY (piped, redirected) or with `--once`, prowl prints
plain tables once and exits — no ANSI, no bell, ASCII glyphs.

Press `Ctrl-C` to quit (no raw mode is used, so it exits cleanly).

## Status palette

The status glyph summarizes each PR using the same Catppuccin Mocha + Nerd Font
set as a tmux window icon. Per PR, the **first** matching state wins:

| State | Glyph | Hex | Picked when | ASCII |
|-------|:-----:|-----|-------------|:-----:|
| merged | `\uE0A0` | `#cba6f7` | PR is merged | `m` |
| conflicts | `\uF071` | `#fab387` | `mergeable == CONFLICTING` | `!` |
| fail | `\uF057` | `#f38ba8` | a check suite that ran is `FAILURE` / `STARTUP_FAILURE` / `CANCELLED` | `x` |
| pending | `\uF111` | `#f9e2af` | a check suite that ran hasn't concluded | `.` |
| pass | `\uF058` | `#a6e3a1` | ≥1 check suite ran, none of the above | `P` |
| none | `-` | dim | no check suites ran | `-` |

The `FAIL` column shows how many check suites actually ran **and** failed (red
when > 0, dim `-` otherwise). Suites with zero check runs — phantom app
subscriptions or workflows that never started, which GitHub's own status rollup
ignores — never count toward the glyph or the `FAIL` total, so a `CLEAN`,
mergeable PR stays green.

The `STATE` column is GitHub's `mergeStateStatus`: `CLEAN` (mergeable, all
green), `UNSTABLE` (mergeable, but non-required checks are failing/pending),
`BLOCKED` (required reviews/checks unmet), `BEHIND`, `DIRTY` (conflict), and
`DRAFT`. The bottom **Reference** legend explains whichever glyphs and states are
currently on screen; hide it with `--no-reference`.

## Change detection / bell

prowl tracks each open PR's status (by number) and the set of merged PR numbers
across refreshes. After the first render, the bell rings **once** when either:

- **a PR of yours merges** — it leaves Open PRs and appears under Merged PRs, or
- **an open PR's status changes** — e.g. pending → pass, or a check starts failing.

Re-sorting, new comments, or a brand-new PR opening do **not** ring. The rows
responsible for a bell are flagged with a `▸` marker on that refresh, and the
status line shows `changed` plus the next refresh time. `--notify` additionally
raises a desktop notification.

## The `gh` queries

Identity and repository (cached once at startup):

```sh
gh api user --jq .login
gh repo view --json nameWithOwner --jq .nameWithOwner   # falls back to:
gh repo set-default --view                              # (worktree-safe; cli/cli#1837)
```

**Merge queue** — variables `owner`, `name`:

```graphql
query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    mergeQueue {
      entries(first: 100) {
        nodes {
          position
          pullRequest { number title url author { login } }
        }
      }
    }
  }
}
```

**My open PRs** — variable `q = "repo:<owner>/<name> is:pr is:open author:<me> archived:false"`:

```graphql
query($q: String!) {
  search(type: ISSUE, first: 50, query: $q) {
    nodes {
      ... on PullRequest {
        number title url state mergeable mergeStateStatus isDraft updatedAt
        mergeQueueEntry { position state }
        commits(last: 1) { nodes { commit { checkSuites(first: 50) { nodes { conclusion checkRuns(first: 1) { totalCount } } } } } }
      }
    }
  }
}
```

**Recently merged** — variable `q = "repo:<owner>/<name> is:pr is:merged author:<me> merged:>=<since> sort:updated-desc"`:

```graphql
query($q: String!) {
  search(type: ISSUE, first: 20, query: $q) {
    nodes {
      ... on PullRequest { number title url mergedAt updatedAt baseRefName }
    }
  }
}
```

## Development

```sh
cargo build            # no warnings
cargo test             # offline, fixture-based (tests/fixtures/)
cargo clippy --all-targets -- -D warnings
```

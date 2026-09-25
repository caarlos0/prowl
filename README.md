# prowl

> A tiny terminal radar for your GitHub pull requests.

<img alt="prowl" width="1300" src="https://github.com/user-attachments/assets/d9dff616-2631-4115-9344-d0c5f42c83ef" />

A tiny terminal dashboard that watches a GitHub repo's **open PRs**, its
**merge queue**, your **recently merged PRs**, and the **commits you've
shipped** per release. It refreshes on an interval and **rings the terminal
bell** the moment one of your PRs merges or an open PR's CI/merge status
changes — and flags whatever changed. On startup it paints instantly from a
local cache, then refreshes in the background. A PR that's in the merge queue is
listed only there, not also under your open PRs.

Press **Tab** to switch to a **reviews** view: the PRs awaiting (or under) your
review — each flagged with a glyph for whether you still owe a first review, the
author asked for a re-review, or there are new commits since you looked — plus
the PRs you reviewed that recently merged. `--review-scope` tunes whether that
list includes only PRs that request you directly or also your teams'.

It talks to the GitHub API directly. On first run it walks you through a
one-time browser **device login** (or set `GITHUB_TOKEN`).

Each open PR leads with **one** Catppuccin-colored glyph answering one question:
did a reviewer **approve** it? Nothing else feeds that glyph — a later change
request does not undo the approval, since the thread count is right there. A PR
that **conflicts** with its base branch marks its own title in red, so it costs
no column. Everything else that could hold a PR back is broken out to the right:
a red/yellow/green **check semaphore** (`FAIL` / `RUN` / `PASS` check-run
counts) and the number of unresolved review **threads**. Nothing is reported
twice.

Merge-queue entries get the same semaphore for their speculative merge commit,
next to how long they've been queued and how long that build has been running.
Use `--required` to count only checks required to merge each pull request; in
the merge queue, `BUILD` then starts at the first required job.

On a TTY prowl uses Nerd Font icons; with `--ascii` (or when piped) approval
falls back to `y` approved and `n` not approved, and the conflict marker to `!`.
`--branch` adds the head branch to every PR table, and `--no-draft`
hides drafts. Each PR number is a clickable link to the PR. Tables use the full
terminal width, giving most space to the title and then the optional branch. As
the terminal gets narrower, detail columns disappear from the right; the
`FAIL` / `RUN` / `PASS` semaphore always disappears as one group.
`+ resize for more` in the footer means a wider or taller terminal would reveal
more information.

## Install

```sh
brew install --cask caarlos0/tap/prowl    # homebrew
npm install -g @caarlos0/prowl            # npm
npx @caarlos0/prowl                       # run without installing
cargo install --path .                    # from source
```

## Login

On first use, prowl runs a one-time GitHub device login and caches the token in
your OS keyring (a `chmod 600` file on Linux/headless). You can also trigger it
explicitly, or skip it entirely with an env var:

```sh
prowl --login                 # authorize once in the browser
GITHUB_TOKEN=… prowl --once    # or just bring your own token
```

## Usage

```sh
prowl                     # watch the repo in the current directory
prowl --repo owner/name   # watch a specific repo
prowl --once              # render once and exit
```

While watching, press `r` to refresh now, `Tab` to switch between your PRs and
your reviews, `?` to toggle the help legend, and `q` (or `Ctrl-C`) to quit;
`Ctrl-Z` suspends it back to your shell. The dashboard takes over the alternate
screen, so quitting hands your shell back exactly as you left it. A footer glued
to the bottom of the screen (`r refresh (every 5m) - tab switch view - enter open
- y copy - / search - ? help`) shows the keys and the refresh interval. While a
refresh is in flight the hint reads `r refreshing` and `r` presses are ignored
until it finishes. On narrow screens the footer removes low-priority labels and
hints instead of clipping. When height is limited, prowl first hides the help legend,
then lower-priority sections. In the PR view it hides shipments, then trims the
merged list from the oldest rows down to the newest row plus a `+N hidden`
line. The merge queue then removes only as many rows as needed, keeping
building PRs first, then your PRs, then other entries. Within each group,
entries nearer the front of the queue stay visible first. The displayed rows
remain in queue order, with a hidden count, and use the available height
before the section disappears. In the reviews view it hides
reviewed-and-merged PRs. The open PR section is always kept whole. If that
section or the minimum useful columns do not fit, prowl shows
`Terminal too small` with the minimum required dimensions. The legend is contextual to the active view:
approval glyphs and the conflict marker for your PRs, review glyphs for your
reviews.

Move the selection cursor through the listed PRs and releases with `j`/`k` (or
`↓`/`↑`), `g`/`G` for the first/last row, and `Ctrl-D`/`Ctrl-U` to jump half a
page; press `Enter` to open the highlighted PR (or release) in your browser. The
cursor only appears once you start moving it, and stays on the same URL when the
terminal is resized if that row remains visible.

Press `y` to copy the selected row's link, or `Y` to copy every link in the
section the cursor is in — your open PRs, the merge queue, the merged list, your
shipments, either reviews list — as a markdown list:

```markdown
- https://github.com/owner/name/pull/1
- https://github.com/owner/name/pull/2
```

With no cursor yet, `Y` copies the first non-empty section; with a filter
applied, it copies only the matching rows. Copying uses the OSC 52 escape, so it
sets the clipboard of the terminal you're looking at even over SSH — as long as
that terminal supports it (in tmux, `set -g set-clipboard on`).

Press `/` to search: type to filter the rows live by number, title, branch,
author, or release tag; `Enter` applies the filter and drops you back to the list (so the
cursor and `Enter` work on the matches), and `Esc` clears it (with no
filter to clear, `Esc` quits).

Run `prowl --help` for all flags (interval, `--only`, `--view`,
`--review-scope`, `--branch`, `--no-draft`, `--required`, merged window, etc.) and the full
watch-mode key list.

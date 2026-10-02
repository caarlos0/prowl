# prowl

> A tiny terminal radar for your GitHub pull requests.

<img alt="prowl" width="1300" src="https://github.com/user-attachments/assets/d9dff616-2631-4115-9344-d0c5f42c83ef" />

A tiny terminal dashboard that watches a GitHub repo's **open PRs**, its
**merge queue**, your **recently merged PRs**, and the **commits you've
shipped** per release. It refreshes on an interval and **rings the terminal
bell** the moment one of your PRs merges or an open PR's CI/merge status
changes — and flags whatever changed until you mark it as read. On startup it
paints instantly from a local cache, then refreshes in the background. A PR
that's in the merge queue is listed only there, not also under your open PRs.

Press **Tab** to switch to a **reviews** view: the PRs awaiting (or under) your
review — each flagged with a glyph for whether you still owe a first review, the
author asked for a re-review, or there are new commits since you looked — plus
the PRs you reviewed that recently merged. `--review-scope` tunes whether that
list includes only PRs that request you directly or also your teams'.

It talks to the GitHub API directly. On first run it walks you through a
one-time browser **device login** (or set `GITHUB_TOKEN`).

Each open PR leads with a Catppuccin-colored approval glyph: a **single check**
means a reviewer approved it, and a **double check** means GitHub reports that
**all required reviews are approved**. Without required reviews, approvals keep
the single check. If more reviews are required or another reviewer requests
changes, an existing approval still earns a single check. A PR
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
falls back to `Y` all required reviews approved, `y` a reviewer approved it, and
`n` nobody approved it. The conflict marker falls back to `!`.
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

## Configuration

prowl reads `~/.config/prowl/config` once at startup. If `XDG_CONFIG_HOME` is
set, it reads `$XDG_CONFIG_HOME/prowl/config` instead; `APPDATA` is the next
fallback, before `~/.config`. Empty environment variables are ignored.

Use a long flag name without `--`, a space, and its value on each line:

```text
interval 30s
bell false
branch true
sort-open created
only mine,queue,merged
link-format [{title}]({url})
```

CLI values override the file, and the file overrides built-in defaults.
Repeated entries use the last value. Boolean values are `true` or `false`;
`--bell=true` enables the bell even when the file says `bell false`.
The existing `--no-bell` still works as an alias for `--bell=false`.

Blank lines and lines starting with `#` are ignored. The rest of each setting
line is its literal value, including spaces and `#`; do not add shell quotes
or expect environment-variable expansion. Leading and trailing whitespace is
ignored. A missing file is fine; invalid settings and unreadable files stop
startup with an error. `--help` and `--version` do not read the file.

## Usage

```sh
prowl                     # watch the repo in the current directory
prowl --repo owner/name   # watch a specific repo
prowl --once              # render once and exit
prowl --sort-open created # newest-created open PRs first
```

**My open PRs** sorts by last update time, newest first, by default.
`--sort-open created` sorts by creation time instead, also newest first.
Set `sort-open created` in the config file to make it the default;
`--sort-open updated` overrides that setting for one run. This option changes
only My open PRs, not the merge queue, merged PRs, or either reviews list.

While watching, press `Ctrl-R` to refresh now, `Tab` to switch between your PRs and
your reviews, `?` to toggle the help legend, and `q` (or `Ctrl-C`) to quit;
`Ctrl-Z` suspends it back to your shell. The dashboard takes over the alternate
screen, so quitting hands your shell back exactly as you left it. A footer glued
to the bottom of the screen (`^R refresh (every 5m) - tab switch view - enter open
- r/R read - y copy - / search - ? help`) shows the keys and the refresh interval.
`^R` means `Ctrl-R`. While a refresh is in flight the hint reads `^R refreshing`
and `Ctrl-R` presses are ignored until it finishes. On narrow screens the footer removes low-priority labels and
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

Press `Shift+Enter` to open every visible link in the selected section, like
`Y` does for copying. It uses the first non-empty section when nothing is
selected and honors the current filter and hidden-row limits. Links open in
display order; if an opener fails, prowl stops opening the remaining links
and shows the failed link.
This shortcut requires a terminal that can report `Shift+Enter` separately
from `Enter`. Prowl enables the Kitty keyboard protocol when supported; if the
terminal sends plain `Enter` instead, only the selected link opens. While
typing a search, either key applies the filter without opening links.

Changed PRs keep their `▸` marker (`>` in ASCII mode) until you press `r` to
mark the selected PR as read, or `R` to mark all PRs as read, including hidden
and filtered rows. Refreshing, moving the selection, opening a PR, switching
views, and failed refreshes do not clear markers. With no PR selected, `r`
does nothing. Markers last only for the current session and track your PRs,
not the Reviews view; `R` clears them from either view. A new event marks the PR
again, and only new events ring the bell.

Press `y` to copy the selected row's link, or `Y` to copy every link in the
section the cursor is in — your open PRs, the merge queue, the merged list, your
shipments, either reviews list — as a markdown list:

```markdown
- https://github.com/owner/name/pull/1
- https://github.com/owner/name/pull/2
```

Use `--link-format` to change both copy commands from URL-only to a custom
format. For Markdown links:

```sh
prowl --link-format '[{title}]({url})'
```

`{title}` is the full PR title, the release tag, or `upcoming`; `{url}` is the
link destination. These placeholders are replaced literally, without Markdown
escaping; all other text is kept as written. The default is `{url}`. Section
copies still prepend `- ` to each formatted link:

```markdown
- [Fix parser](https://github.com/owner/name/pull/1)
- [Update docs](https://github.com/owner/name/pull/2)
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
`--review-scope`, `--branch`, `--no-draft`, `--required`, `--link-format`,
merged window, etc.) and the full watch-mode key list.

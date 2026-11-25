# prowl

[![build](https://github.com/caarlos0/prowl/actions/workflows/build.yml/badge.svg)](https://github.com/caarlos0/prowl/actions/workflows/build.yml)
[![release](https://img.shields.io/github/v/release/caarlos0/prowl)](https://github.com/caarlos0/prowl/releases/latest)

A tiny terminal radar for your GitHub pull requests.

<img alt="prowl" width="1300" src="https://github.com/user-attachments/assets/2933f2fb-38a1-487c-9022-85d3b54a8cdc" />

## Why prowl

Keep one terminal pane open and know what needs you now. prowl shows your open
PRs with their CI and review state, the merge queue, what you merged and which
release shipped it, and the PRs waiting for your review. It rings the terminal
bell when one of your PRs merges or changes status. It starts instantly from a
local cache, then refreshes in the background.

## Features

- Two views, **Mine** and **Reviews**. Press `Tab` to switch.
- A check semaphore (`FAIL` / `RUN` / `PASS`) that also counts required checks that have not reported yet.
- Approval glyphs and a conflict marker, so you see what blocks a PR at a glance.
- Merge Queue with `WAIT`, `BUILD`, and the queue ETA.
- My Shipments (your commits per release) and a `RELEASE` column on merged PRs.
- Terminal bell and unread markers. Press `r` / `R` to mark them as read.
- Keyboard and mouse support, live `/` search, open in the browser, and link copy that works over SSH.
- Instant start from cache, a layout that adapts to the terminal width, and plain output when piped.

## Install

Homebrew:

```sh
brew install --cask caarlos0/tap/prowl
```

npm:

```sh
npm install -g @caarlos0/prowl
npx @caarlos0/prowl   # or run it without installing
```

Linux packages (`.apk`, `.deb`, `.rpm`) and prebuilt archives (`.tar.gz`, or
`.zip` on Windows) are on the
[releases page](https://github.com/caarlos0/prowl/releases). Archives cover
Linux and macOS on x86_64 and arm64, and Windows on x86_64.

From source:

```sh
cargo install --path .
```

A [Nerd Font](https://www.nerdfonts.com/) is recommended for the icons. Without
one, pass `--ascii`.

## Quick start

```sh
prowl                     # watch the repo in the current directory
prowl --repo owner/name   # watch a specific repo
prowl --once              # print one snapshot and exit
```

On first run, prowl starts a GitHub device login in your browser.

## Authentication

prowl looks for a token in this order:

1. `PROWL_TOKEN`
2. `GITHUB_TOKEN`
3. The OS keyring, or a `chmod 600` file when no keyring is available
4. The OAuth device flow

`prowl --login` forces the device flow and stores the new token.

## Views

### Mine

The default view has four sections:

- **My open PRs**: sorted by last update, newest first. Use `--sort-open created` to sort by creation time. A PR in the merge queue is listed only under Merge Queue.
- **Merge Queue**: each entry shows `WAIT` (time in the queue), `BUILD` (time its merge commit has been building, `—` until a job starts), and its own check semaphore.
- **My merged PRs**: recently merged PRs and the release that shipped each one. `—` means not released yet, `?` means unknown.
- **My Shipments**: how many commits you shipped in the next release and in the last four.

Each open PR shows:

| Column | Meaning |
| --- | --- |
| Approval glyph | Double check (`Y` in ASCII): GitHub reports all required reviews approved. Single check (`y`): a reviewer approved. Pending glyph (`n`): nobody approved. |
| Red marker on the title (`!` in ASCII) | The PR conflicts with its base branch. |
| `THREADS` | Unresolved review threads (`100+` when capped). |
| `FAIL` / `RUN` / `PASS` | Check counts. `RUN` includes queued and waiting checks, and required checks that have not reported yet. |

Use `--required` to count only the checks required to merge. Use `--branch` to
add a `BRANCH` column and `--no-draft` to hide drafts.

### Reviews

- **Reviews**: open PRs that wait for you or that you already reviewed. By default prowl includes PRs that request you directly or through one of your teams. Use `--review-scope direct` for direct requests only.
- **Reviewed & merged**: merged PRs that you reviewed.

Each review row starts with its state:

| State | Meaning |
| --- | --- |
| `Awaiting` | You are asked to review, and you have not yet. |
| `ReReview` | You reviewed, and you are asked again. |
| `Updated` | You reviewed, and the author pushed new commits. |
| `Reviewed` | You reviewed, and nothing new happened. |

When a search is capped at 50 PRs, the header shows how many are listed and how
many exist.

## Keys and mouse

| Key | Action |
| --- | --- |
| `j` / `k`, `↓` / `↑` | Move the selection |
| `g` / `G` | Jump to the first / last row |
| `Ctrl-D` / `Ctrl-U` | Move the selection half a page |
| `PgUp` / `PgDn` | Scroll the viewport without changing the selection |
| `Enter` | Open the selected PR or release in your browser |
| `Shift+Enter` | Open every link in the selected section |
| `y` / `Y` | Copy the selected link / every link in the section |
| `/` | Filter by number, title, branch, author, or release tag |
| `Esc` | Clear the filter (quits when there is none) |
| `r` / `R` | Mark the selected PR / all PRs as read |
| `Ctrl-R` | Refresh now |
| `Tab` | Switch view |
| `?` | Toggle the help legend |
| `q` / `Ctrl-C` | Quit |
| `Ctrl-Z` | Suspend to your shell |

Use the mouse wheel to scroll. Click a row to select it, click a PR number,
release tag, or `upcoming` to open that link, and click a tab to switch views.
Most terminals let you hold `Shift` to select text.

`Shift+Enter` needs a terminal that supports the Kitty keyboard protocol.
Otherwise it acts like `Enter`. Copying uses OSC 52. In tmux, set
`set -g set-clipboard on`.

By default `y` copies the plain URL. Use `--link-format '[{title}]({url})'` to
copy Markdown links. `{title}` is the PR title, release tag, or `upcoming`, and
`{url}` is the link.

The active view refreshes on every interval and the other view on every second
one. A dim `updated 6m ago` line shows when the view you see is older than the
other.

## Configuration

prowl reads a config file once at startup, from the first of these that is set:

1. `$XDG_CONFIG_HOME/prowl/config`
2. `$APPDATA/prowl/config`
3. `~/.config/prowl/config`

Write one setting per line: a long flag name without `--`, a space, and its
value.

```text
# refresh every 30 seconds, without the bell
interval 30s
bell false
branch true
sort-open created
only mine,queue,merged
link-format [{title}]({url})
```

CLI values override the file, and the file overrides the defaults. Repeated
entries use the last value. Blank lines and lines that start with `#` are
ignored. The rest of the line is the literal value, so do not add shell quotes.
A missing file is fine. An invalid setting stops startup with an error that
names the file and line. `--help` and `--version` never read the file.

## Flags

Run `prowl --help` for the full list.

| Flag | Description |
| --- | --- |
| `--repo <OWNER/NAME>` | Repository to watch. Detected from the current directory by default. |
| `--interval <DUR>` | Refresh interval, such as `30s`, `10m`, `2h`. Default `5m`. |
| `--once` | Render once and exit. |
| `--view <mine\|reviews>` | View to show first. |
| `--only <SECTIONS>` | Comma-separated sections: `queue`, `mine`, `merged`, `shipments`. |
| `--review-scope <direct\|all>` | Whose review requests the Reviews view includes. Default `all`. |
| `--sort-open <updated\|created>` | Sort order of My open PRs. Default `updated`. |
| `--branch` | Show each PR's head branch. |
| `--no-draft` | Hide draft PRs. |
| `--required` | Count only the checks required to merge. |
| `--merged-window <DUR>` | How far back merged PRs reach. Default `2d`. |
| `--merged-limit <N>` | Maximum merged PRs to list. Default `20`. |
| `--include-pre-releases` | Count pre-releases in My Shipments. |
| `--ascii` | Use ASCII letters instead of Nerd Font glyphs. |
| `--bell=false` | Turn off the terminal bell. `--no-bell` is an alias. |
| `--link-format <FORMAT>` | Format of copied links. Default `{url}`. |
| `--no-cache` | Do not read or write the on-disk cache. |
| `--no-help` | Hide the help legend in one-shot and piped output. |
| `--login` | Run the GitHub device login and exit. |

## Piped output and scripting

With `--once`, prowl prints one snapshot and exits. When the output is piped or
redirected, it is plain text without colors, 120 columns wide, with every row.
On a terminal, PR numbers and release tags are hyperlinks.

## Contributing and license

See [CONTRIBUTING.md](CONTRIBUTING.md). prowl is released under the MIT license;
see [LICENSE.md](LICENSE.md).

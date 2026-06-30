# prowl

> A tiny terminal radar for your GitHub pull requests.

<img width="1788" height="1662" alt="CleanShot 2026-06-20 at 00 25 09" src="https://github.com/user-attachments/assets/72f0fa20-93f3-44fd-ac24-966cfac55c36" />

A tiny terminal dashboard that watches a GitHub repo's **open PRs**, its
**merge queue**, your **recently merged PRs**, and the **commits you've
shipped** per release. It refreshes on an interval and **rings the terminal
bell** the moment one of your PRs merges or an open PR's CI/merge status
changes — and flags whatever changed. On startup it paints instantly from a
local cache, then refreshes in the background.

Press **Tab** to switch to a **reviews** view: the PRs awaiting (or under) your
review — each flagged with a glyph for whether you still owe a first review, the
author asked for a re-review, or there are new commits since you looked — plus
the PRs you reviewed that recently merged. `--review-scope` tunes whether that
list includes only PRs that request you directly or also your teams'.

It talks to the GitHub API directly. On first run it walks you through a
one-time browser **device login** (or set `GITHUB_TOKEN`).

Status is a single Catppuccin-colored glyph. On a TTY, prowl uses Nerd Font
icons (pass, fail, pending, conflicts, merged); with `--ascii` (or when piped)
it falls back to `P` pass, `x` fail, `.` pending, `!` conflicts, `m` merged.
Each PR number is a clickable link to the PR. Long titles are truncated (with a
`⋯`) and the whole view is kept within 120 columns.

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
your reviews, `?` to toggle the help legend, and `Ctrl-C` to quit. A footer at
the bottom (`r refresh (every 5m) - tab switch view - ? help`) shows the keys
and the refresh interval.
The legend is contextual to the active view: status glyphs and `STATE` values
for your PRs, review glyphs for your reviews.

Run `prowl --help` for all flags (interval, `--only`, `--view`,
`--review-scope`, merged window, etc.).

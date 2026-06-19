# prowl

> A tiny terminal radar for your GitHub pull requests.

A tiny terminal dashboard that watches a GitHub repo's **open PRs**, its
**merge queue**, your **recently merged PRs**, and the **commits you've
shipped** per release. It refreshes on an interval and **rings the terminal
bell** the moment one of your PRs merges or an open PR's CI/merge status
changes — and flags whatever changed. On startup it paints instantly from a
local cache, then refreshes in the background.

It talks to the GitHub API directly. On first run it walks you through a
one-time browser **device login** (or set `GITHUB_TOKEN`) — no `gh` CLI needed.

```
▌ Open PRs  3
   P  #6656  feat: goreleaser check --allow-deprecations  CLEAN      -  -  …/pull/6656
 ▸ !  #6475  feat: install scripts                        CONFLICTS  -  3  …/pull/6475

▌ Merged PRs  1
   m  #6649  fix(mcp): clean subfolder path               main  1w   …/pull/6649

▌ Shipped
  upcoming: 3
    v1.2.0: 8
    v1.1.0: 5
    v1.0.0: 12
    v0.9.0: 4

updated 11:21:16 — changed · next 11:26:16
```

Status is a single Catppuccin-colored Nerd Font glyph (`P` pass, `x` fail,
`.` pending, `!` conflicts, `m` merged); URLs are clickable. Pass `--ascii` if
your terminal has no Nerd Font.

## Install

```sh
cargo install --path .
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

While watching, press `r` to refresh now and `Ctrl-C` to quit.

Run `prowl --help` for all flags (interval, `--only`, merged window, etc.).

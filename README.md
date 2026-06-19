# prowl

A tiny terminal dashboard that watches a GitHub repo's **open PRs**, its
**merge queue**, and your **recently merged PRs**. It refreshes on an interval
and **rings the terminal bell** the moment one of your PRs merges or an open
PR's CI/merge status changes — and flags whatever changed.

It shells out to the [`gh`](https://cli.github.com) CLI, so it just uses your
existing GitHub auth — no tokens, no config.

```
▌ Open PRs  3
   P  #6656  feat: goreleaser check --allow-deprecations  CLEAN    -  -  …/pull/6656
 ▸ !  #6475  feat: install scripts                        DIRTY    -  3  …/pull/6475

▌ Merged PRs  1
   m  #6649  fix(mcp): clean subfolder path               main  1w   …/pull/6649

updated 11:21:16 — changed · next 11:26:16
```

Status is a single Catppuccin-colored Nerd Font glyph (`P` pass, `x` fail,
`.` pending, `!` conflicts, `m` merged); URLs are clickable. Pass `--ascii` if
your terminal has no Nerd Font.

## Install

```sh
cargo install --path .
```

Requires the [`gh`](https://cli.github.com) CLI on your `PATH`, authenticated
(`gh auth login`).

## Usage

```sh
prowl                     # watch the repo in the current directory
prowl --repo owner/name   # watch a specific repo
prowl --once              # render once and exit
```

Run `prowl --help` for all flags (interval, `--only`, merged window, etc.).

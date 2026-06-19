# Contributing

Thanks for your interest in prowl!

## Prerequisites

- A recent Rust toolchain (the crate uses edition 2024; Rust 1.95+).
- The [`gh`](https://cli.github.com) CLI, authenticated (`gh auth login`) — prowl
  shells out to it, and the live `--once` run uses it.

## Build, run, test, lint

```sh
cargo build
cargo run -- --repo owner/name --once
cargo test                                   # offline; uses tests/fixtures/
cargo clippy --all-targets -- -D warnings
```

All four must be green before you open a PR: the build is warning-free,
clippy is clean with `-D warnings`, and the tests pass without network access.

## Conventions

- **[Conventional Commits](https://www.conventionalcommits.org/)** with a scope
  when it helps, e.g. `fix(status): ignore zero-run check suites`.
- **One logical change per commit.** Keep diffs small and focused.
- **Sign off** your commits (`git commit -s`).
- **Merge, don't rebase,** when integrating an upstream branch (e.g. `main`)
  into a feature branch — preserve merge topology.
- Prefer the simplest solution. No defensive code (retries, timeouts, guards)
  without evidence the problem exists. Verify a bug is real before fixing it.
- Only comment code that genuinely needs clarification.

## Tests

Tests run fully offline against JSON fixtures captured from `gh api graphql`
(`tests/fixtures/`). When you change a GraphQL query or the parsing/derivation
logic, re-capture or hand-edit the relevant fixture and update the assertions in
`tests/parsing.rs` (and the per-module unit tests).

## Keeping docs current

If you change behavior, flags, queries, or architecture, update `README.md` and
`AGENTS.md` in the same change.

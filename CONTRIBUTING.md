# Contributing

Thanks for your interest in prowl!

## Prerequisites

- A recent Rust toolchain (the crate uses edition 2024; Rust 1.95+).
- A GitHub token for live runs: set `PROWL_TOKEN` or `GITHUB_TOKEN`, or run
  `prowl --login` once for the browser device flow. No `gh` CLI needed.

## Build, run, test, lint

```sh
cargo build
cargo run -- --repo owner/name --once
cargo test                                   # offline; uses tests/fixtures/
cargo clippy --all-targets -- -D warnings
```

All four must be green before you open a PR: the build is warning-free,
clippy is clean with `-D warnings`, and the tests pass without network access.
`task ci` runs all of them.

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

Tests run fully offline against JSON fixtures captured from the GitHub GraphQL
API (`tests/fixtures/`). When you change a GraphQL query or the parsing/derivation
logic, re-capture or hand-edit the relevant fixture and update the assertions in
`tests/parsing.rs` (and the per-module unit tests).

## The README screenshot

The README image is generated from made-up data — never from a real repo. The
`demo` example feeds a fake `Sections` through the same `render_to_string` the
binary uses, so the shot can't drift from the real layout:

```sh
task screenshot                      # regenerate, upload, and relink it

cargo run --example demo             # or just print the "my PRs" view
cargo run --example demo -- reviews  # ...and the reviews view
cargo run --example demo -- scroll   # top/bottom eight-row watch frames
cargo run --example demo -- reviews scroll
cargo run --example demo -- mouse    # simulated row/link clicks and wheel input
```

`task screenshot` needs [vhs](https://github.com/charmbracelet/vhs) (and the
Nerd Font named in `demo.tape`), `gh`, `jq` and `curl`. It renders `demo.png`,
uploads it to GitHub's CDN, and rewrites the `<img>` URL in `README.md` —
`demo.png` itself is gitignored, so no binary lands in the repo. Edit
`examples/demo.rs` to change what the shot shows.

An uploaded asset is private until its URL appears in a *comment, issue or PR
body* — committing it in a tracked file does not count (tested: still 404 four
minutes later, so a README-only reference renders a broken image). The task
therefore posts the URL in a commit comment and deletes that comment again,
which publishes the asset for good, and then polls the URL anonymously until it
answers 200 before touching the README.

## Keeping docs current

If you change behavior, flags, queries, or architecture, update `README.md` and
`AGENTS.md` in the same change.

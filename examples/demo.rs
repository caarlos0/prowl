//! Renders the dashboard from fake data, for the README screenshot.
//!
//! Real output can't be published (it's whatever repo you're watching), so this
//! feeds a made-up `Sections` through the same `render_to_string` the binary
//! uses — the shot can't drift from the real layout. Timestamps are relative to
//! now, so the ages stay sensible whenever it's regenerated.
//!
//! ```sh
//! cargo run --quiet --example demo            # the "my PRs" view
//! cargo run --quiet --example demo -- reviews # the reviews view
//! vhs demo.tape                               # write demo.png
//! ```

use chrono::{Duration, SecondsFormat, Utc};
use clap::Parser;
use prowl::Ui;
use prowl::changes::Changes;
use prowl::cli::{Cli, View};
use prowl::commits::{Bucket, CommitStats, Count, Release, ReleaseRef};
use prowl::merged::MergedRow;
use prowl::prs::PrRow;
use prowl::queue::QueueRow;
use prowl::reviews::{ReviewRow, ReviewedMergedRow};
use prowl::status::{Approval, Checks, ReviewState, Status};
use uncurses::color::Profile;

const REPO: &str = "acme/rocket";

fn url(number: i64) -> String {
    format!("https://github.com/{REPO}/pull/{number}")
}

fn release_url(tag: &str) -> String {
    format!("https://github.com/{REPO}/releases/tag/{tag}")
}

/// An RFC 3339 timestamp `mins` minutes in the past.
fn ago(mins: i64) -> Option<String> {
    Some((Utc::now() - Duration::minutes(mins)).to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn checks(fail: u64, running: u64, pass: u64) -> Checks {
    Checks {
        fail,
        running,
        pass,
    }
}

/// A green, quiet open PR — rows below override only what makes them interesting.
fn open_pr(number: i64, title: &str, branch: &str) -> PrRow {
    PrRow {
        number,
        is_draft: false,
        title: title.into(),
        branch: branch.into(),
        approval: Approval::Approved,
        conflicts: false,
        status: Some(Status::Pass),
        checks: checks(0, 0, 14),
        unresolved: 0,
        unresolved_capped: false,
        queue: None,
        url: url(number),
        updated_at: ago(0),
        created_at: ago(24 * 60),
    }
}

/// A queue entry that just joined and hasn't started building.
fn entry(position: i64, number: i64, author: &str, title: &str) -> QueueRow {
    QueueRow {
        position,
        number,
        author: author.into(),
        title: title.into(),
        branch: format!("feature/pr-{number}"),
        url: url(number),
        mine: false,
        enqueued_at: ago(0),
        build_started_at: None,
        checks: checks(0, 0, 0),
    }
}

fn merged(number: i64, title: &str, release: Option<&str>, merged_mins: i64) -> MergedRow {
    MergedRow {
        number,
        title: title.into(),
        branch: format!("merged/pr-{number}"),
        url: url(number),
        release: release.map(|tag| ReleaseRef {
            tag: tag.into(),
            url: release_url(tag),
        }),
        merged_at: ago(merged_mins),
    }
}

fn bucket(mine: usize, url: String) -> Bucket {
    Bucket {
        count: Count {
            mine,
            capped: false,
        },
        url,
    }
}

fn sections() -> prowl::Sections {
    prowl::Sections {
        prs: Some(vec![
            PrRow {
                approval: Approval::RequiredApproved,
                updated_at: ago(7),
                ..open_pr(
                    412,
                    "feat(queue): show how long the speculative build has been running",
                    "queue-build-age",
                )
            },
            PrRow {
                approval: Approval::Pending,
                status: Some(Status::Fail),
                checks: checks(1, 2, 11),
                unresolved: 3,
                updated_at: ago(52),
                ..open_pr(
                    408,
                    "fix(render): keep the footer pinned when the terminal is short",
                    "pin-footer",
                )
            },
            PrRow {
                approval: Approval::Pending,
                conflicts: true,
                status: Some(Status::Conflicts),
                unresolved: 1,
                updated_at: ago(260),
                ..open_pr(
                    401,
                    "refactor(status): split the open-PR glyph into approval and conflicts",
                    "two-glyphs",
                )
            },
            PrRow {
                status: Some(Status::Pending),
                checks: checks(0, 4, 10),
                updated_at: ago(1_500),
                ..open_pr(396, "chore(deps): bump rustls to 0.23", "bump-rustls")
            },
        ]),
        queue: Some(vec![
            QueueRow {
                enqueued_at: ago(26),
                build_started_at: ago(21),
                checks: checks(0, 0, 14),
                ..entry(
                    1,
                    399,
                    "monalisa",
                    "perf(engine): reuse the parsed rollup between refreshes",
                )
            },
            QueueRow {
                mine: true,
                enqueued_at: ago(12),
                build_started_at: ago(4),
                checks: checks(0, 3, 9),
                ..entry(2, 405, "octocat", "feat(api): paginate the review threads")
            },
            QueueRow {
                enqueued_at: ago(3),
                ..entry(3, 411, "hubot", "docs: document the merge queue columns")
            },
        ]),
        queue_next_eta: Some(11 * 60),
        merged: Some(vec![
            merged(
                394,
                "feat(cache): paint the last snapshot on startup",
                None,
                180,
            ),
            merged(
                389,
                "fix(term): restore the cursor on SIGTERM",
                Some("v1.4.0"),
                1_600,
            ),
            merged(
                382,
                "feat(nav): open the selected row in the browser",
                Some("v1.4.0"),
                2_900,
            ),
            merged(
                377,
                "fix(prs): drop queued PRs from the open list",
                Some("v1.3.2"),
                6_100,
            ),
        ]),
        commits: Some(CommitStats {
            available: true,
            upcoming: Some(bucket(
                9,
                format!("https://github.com/{REPO}/compare/v1.4.0...main"),
            )),
            releases: vec![
                Release {
                    tag: "v1.4.0".into(),
                    bucket: bucket(23, release_url("v1.4.0")),
                    published_at: ago(4 * 24 * 60),
                },
                Release {
                    tag: "v1.3.2".into(),
                    bucket: bucket(4, release_url("v1.3.2")),
                    published_at: ago(12 * 24 * 60),
                },
                Release {
                    tag: "v1.3.1".into(),
                    bucket: bucket(7, release_url("v1.3.1")),
                    published_at: ago(20 * 24 * 60),
                },
                Release {
                    tag: "v1.3.0".into(),
                    bucket: bucket(31, release_url("v1.3.0")),
                    published_at: ago(33 * 24 * 60),
                },
            ],
        }),
        reviews: Some(vec![
            ReviewRow {
                number: 415,
                is_draft: false,
                title: "feat(auth): store the token in the OS keyring".into(),
                branch: "keyring-token".into(),
                author: "monalisa".into(),
                url: url(415),
                state: ReviewState::Awaiting,
                updated_at: ago(18),
            },
            ReviewRow {
                number: 409,
                is_draft: false,
                title: "fix(github): surface GraphQL errors instead of empty data".into(),
                branch: "graphql-errors".into(),
                author: "hubot".into(),
                url: url(409),
                state: ReviewState::ReReview,
                updated_at: ago(95),
            },
            ReviewRow {
                number: 402,
                is_draft: false,
                title: "feat(render): truncate long titles to fit 120 columns".into(),
                branch: "responsive-tables".into(),
                author: "robotocat".into(),
                url: url(402),
                state: ReviewState::Updated,
                updated_at: ago(410),
            },
            ReviewRow {
                number: 397,
                is_draft: false,
                title: "chore(ci): run clippy with -D warnings".into(),
                branch: "clippy-warnings".into(),
                author: "monalisa".into(),
                url: url(397),
                state: ReviewState::Reviewed,
                updated_at: ago(1_900),
            },
        ]),
        reviewed_merged: Some(vec![
            ReviewedMergedRow {
                number: 392,
                title: "feat(clipboard): copy links with OSC 52".into(),
                branch: "osc52-copy".into(),
                author: "octocat".into(),
                url: url(392),
                merged_at: ago(700),
            },
            ReviewedMergedRow {
                number: 386,
                title: "fix(queue): count checks from the rollup aggregates".into(),
                branch: "queue-check-counts".into(),
                author: "hubot".into(),
                url: url(386),
                merged_at: ago(2_400),
            },
        ]),
    }
}

fn main() {
    let view = match std::env::args().nth(1).as_deref() {
        Some("reviews") => View::Reviews,
        _ => View::Mine,
    };
    let cli = Cli::parse_from(["prowl"]);

    // A few rows carry the unread change marker and one is
    // selected, so the shot shows both affordances.
    let changes = Changes {
        status_changed: [408, 412].into_iter().collect(),
        newly_merged: [394].into_iter().collect(),
    };

    let ui = Ui {
        view,
        selected: Some(1),
        show_help: true,
        ..Ui::once(&cli)
    };
    println!(
        "{}",
        prowl::render_to_string(
            &sections(),
            &ui,
            &changes,
            Some(("5m", false)),
            false,
            Profile::TrueColor,
        )
    );
}

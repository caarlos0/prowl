//! Shared status palette — the single source of truth for the CI/PR status
//! indicator, matching caarlos0's `tmux-window-icon` script. Catppuccin Mocha
//! colors, Nerd Font glyphs, 24-bit truecolor.

use crate::model::{CheckSuite, CheckSuites, PrNode};
use anstyle::{RgbColor, Style};

pub type Rgb = (u8, u8, u8);

// Catppuccin Mocha palette (the subset the dashboard uses).
pub const GREEN: Rgb = (166, 227, 161); // #a6e3a1
pub const RED: Rgb = (243, 139, 168); // #f38ba8
pub const YELLOW: Rgb = (249, 226, 175); // #f9e2af
pub const MAUVE: Rgb = (203, 166, 247); // #cba6f7
pub const PEACH: Rgb = (250, 179, 135); // #fab387
pub const BLUE: Rgb = (137, 180, 250); // #89b4fa
pub const LAVENDER: Rgb = (180, 190, 254); // #b4befe
pub const TEAL: Rgb = (148, 226, 213); // #94e2d5
pub const PINK: Rgb = (245, 194, 231); // #f5c2e7 — "changed since last refresh" marker

/// CI/PR status. Glyphs/colors are fixed by the shared palette.
#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Status {
    Merged,
    Conflicts,
    Fail,
    Pending,
    Pass,
}

/// All statuses in legend order.
pub const ORDER: [Status; 5] = [
    Status::Pass,
    Status::Fail,
    Status::Pending,
    Status::Conflicts,
    Status::Merged,
];

/// `mergeStateStatus` values in legend order.
pub const STATE_ORDER: [&str; 8] = [
    "CLEAN",
    "UNSTABLE",
    "BLOCKED",
    "BEHIND",
    "DIRTY",
    "DRAFT",
    "HAS_HOOKS",
    "UNKNOWN",
];

/// Glyph + truecolor for a status — the single lookup both views share.
pub fn status_style(s: Status) -> (char, Rgb) {
    match s {
        Status::Pass => ('\u{F058}', GREEN),
        Status::Fail => ('\u{F057}', RED),
        Status::Pending => ('\u{F111}', YELLOW),
        Status::Merged => ('\u{E0A0}', MAUVE),
        Status::Conflicts => ('\u{F071}', PEACH),
    }
}

/// ASCII fallback letter for a status (non-Nerd-Font terminals / piped output).
pub fn status_ascii(s: Status) -> char {
    match s {
        Status::Pass => 'P',
        Status::Fail => 'x',
        Status::Pending => '.',
        Status::Merged => 'm',
        Status::Conflicts => '!',
    }
}

/// The glyph to render, honoring the ASCII toggle.
pub fn glyph(s: Status, ascii: bool) -> char {
    if ascii {
        status_ascii(s)
    } else {
        status_style(s).0
    }
}

/// One-line meaning of a status (for the reference legend).
pub fn status_meaning(s: Status) -> &'static str {
    match s {
        Status::Pass => "all checks that ran passed",
        Status::Fail => "a check that ran failed",
        Status::Pending => "checks still running",
        Status::Merged => "merged",
        Status::Conflicts => "merge conflict; needs a rebase",
    }
}

/// One-line meaning of a `mergeStateStatus` value (for the reference legend).
pub fn state_meaning(state: &str) -> &'static str {
    match state {
        "CLEAN" => "mergeable; all required checks green",
        "UNSTABLE" => "mergeable, but non-required checks are failing or pending",
        "BLOCKED" => "blocked; required reviews or checks not satisfied",
        "BEHIND" => "behind the base branch; needs an update",
        "DIRTY" => "merge conflict",
        "DRAFT" => "draft; not ready to merge",
        "HAS_HOOKS" => "mergeable, with pre-receive hooks",
        "UNKNOWN" => "mergeability not yet computed",
        _ => "",
    }
}

/// Truecolor style for a `mergeStateStatus` value, using the shared palette.
pub fn state_style(state: &str) -> Style {
    match state {
        "CLEAN" | "HAS_HOOKS" => fg(GREEN),
        "UNSTABLE" | "BLOCKED" | "BEHIND" => fg(YELLOW),
        "DIRTY" | "DRAFT" => fg(RED),
        _ => Style::new().dimmed(),
    }
}

/// Display label for a `mergeStateStatus` value. GitHub's `DIRTY` reads as
/// `CONFLICTS`; everything else is shown verbatim.
pub fn state_label(state: &str) -> &str {
    match state {
        "DIRTY" => "CONFLICTS",
        other => other,
    }
}

/// Nerd Font glyph for a `mergeStateStatus` value (FontAwesome range, so it
/// renders in any Nerd Font). Used on a TTY; `--ascii`/piped output falls back
/// to [`state_label`].
pub fn state_glyph(state: &str) -> char {
    match state {
        "CLEAN" | "HAS_HOOKS" => '\u{f00c}', // check
        "UNSTABLE" => '\u{f06a}',            // exclamation-circle
        "BLOCKED" => '\u{f023}',             // lock
        "BEHIND" => '\u{f063}',              // arrow-down
        "DIRTY" => '\u{f127}',               // broken link (conflict)
        "DRAFT" => '\u{f040}',               // pencil
        _ => '\u{f128}',                     // question mark
    }
}

/// A truecolor foreground style.
pub fn fg(rgb: Rgb) -> Style {
    Style::new().fg_color(Some(RgbColor(rgb.0, rgb.1, rgb.2).into()))
}

/// Check-suite conclusions that count as a failure.
pub const FAIL_CONCLUSIONS: [&str; 5] = [
    "FAILURE",
    "STARTUP_FAILURE",
    "CANCELLED",
    "TIMED_OUT",
    "ACTION_REQUIRED",
];

/// Terminal-failure conclusions that are genuine even with zero runs: the suite
/// failed before producing any check run, so the phantom filter must not mask
/// it. A zero-run `FAILURE`/`CANCELLED`, by contrast, is a phantom subscription.
const TERMINAL_FAIL_CONCLUSIONS: [&str; 1] = ["STARTUP_FAILURE"];

/// Whether a check suite actually ran (produced ≥1 check run). Zero-run suites
/// are phantom subscriptions GitHub ignores, so we do too; a `null` run count
/// (an inaccessible suite) is treated the same way.
fn ran(s: &CheckSuite) -> bool {
    s.check_runs.as_ref().is_some_and(|r| r.total_count > 0)
}

/// Count the check suites that concluded in a failing state. A suite counts if
/// it ran, or if it concluded in a terminal failure that legitimately produces
/// no runs (e.g. a zero-run `STARTUP_FAILURE`), which the phantom filter must
/// not mask.
pub fn fail_count(suites: &[CheckSuite]) -> usize {
    suites
        .iter()
        .filter(|s| {
            s.conclusion.as_deref().is_some_and(|c| {
                FAIL_CONCLUSIONS.contains(&c) && (ran(s) || TERMINAL_FAIL_CONCLUSIONS.contains(&c))
            })
        })
        .count()
}

/// Derive a PR's status with the precedence
/// `merged > conflicts > fail > pending > pass > none`. Only check suites that
/// actually ran are considered, so empty/phantom suites never turn a green PR
/// red (or yellow).
pub fn derive_status(
    state: Option<&str>,
    mergeable: Option<&str>,
    suites: &[CheckSuite],
) -> Option<Status> {
    if state == Some("MERGED") {
        return Some(Status::Merged);
    }
    if mergeable == Some("CONFLICTING") {
        return Some(Status::Conflicts);
    }
    if fail_count(suites) > 0 {
        return Some(Status::Fail);
    }
    if suites
        .iter()
        .filter(|s| ran(s))
        .any(|s| s.conclusion.is_none())
    {
        return Some(Status::Pending);
    }
    if suites.iter().any(ran) {
        return Some(Status::Pass);
    }
    None
}

/// The check suites of a PR's last commit (empty if none).
pub fn last_suites(pr: &PrNode) -> &[CheckSuite] {
    last_check_suites(pr)
        .map(|s| s.nodes.as_slice())
        .unwrap_or(&[])
}

/// The last commit's check suites, with the server-reported total.
fn last_check_suites(pr: &PrNode) -> Option<&CheckSuites> {
    pr.commits.nodes.first().map(|c| &c.commit.check_suites)
}

/// Derive a PR node's status from its fields.
pub fn pr_status(pr: &PrNode) -> Option<Status> {
    let status = derive_status(
        pr.state.as_deref(),
        pr.mergeable.as_deref(),
        last_suites(pr),
    );
    // We only fetch the first page of check suites; if the server reports more
    // than we received, a dropped suite could be failing — a "pass" is unproven,
    // so surface it as pending rather than a false green.
    if status == Some(Status::Pass)
        && last_check_suites(pr).is_some_and(|s| s.total_count > s.nodes.len() as u64)
    {
        return Some(Status::Pending);
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CheckRuns;

    fn suites(concls: &[Option<&str>]) -> Vec<CheckSuite> {
        concls
            .iter()
            .map(|c| CheckSuite {
                conclusion: c.map(str::to_string),
                check_runs: Some(CheckRuns { total_count: 1 }),
            })
            .collect()
    }

    /// A check suite with an explicit run count (0 = phantom).
    fn suite(conclusion: Option<&str>, runs: u64) -> CheckSuite {
        CheckSuite {
            conclusion: conclusion.map(str::to_string),
            check_runs: Some(CheckRuns { total_count: runs }),
        }
    }

    #[test]
    fn palette_glyphs_and_colors_are_exact() {
        assert_eq!(status_style(Status::Pass), ('\u{F058}', (166, 227, 161)));
        assert_eq!(status_style(Status::Fail), ('\u{F057}', (243, 139, 168)));
        assert_eq!(status_style(Status::Pending), ('\u{F111}', (249, 226, 175)));
        assert_eq!(status_style(Status::Merged), ('\u{E0A0}', (203, 166, 247)));
        assert_eq!(
            status_style(Status::Conflicts),
            ('\u{F071}', (250, 179, 135))
        );
    }

    #[test]
    fn ascii_letters() {
        assert_eq!(status_ascii(Status::Pass), 'P');
        assert_eq!(status_ascii(Status::Fail), 'x');
        assert_eq!(status_ascii(Status::Pending), '.');
        assert_eq!(status_ascii(Status::Merged), 'm');
        assert_eq!(status_ascii(Status::Conflicts), '!');
    }

    #[test]
    fn precedence_is_respected() {
        // merged beats everything, even conflicts + failures.
        let s = suites(&[Some("FAILURE")]);
        assert_eq!(
            derive_status(Some("MERGED"), Some("CONFLICTING"), &s),
            Some(Status::Merged)
        );
        // conflicts beats failing checks.
        assert_eq!(
            derive_status(Some("OPEN"), Some("CONFLICTING"), &s),
            Some(Status::Conflicts)
        );
        // fail beats pending.
        let s = suites(&[Some("FAILURE"), None]);
        assert_eq!(
            derive_status(Some("OPEN"), Some("MERGEABLE"), &s),
            Some(Status::Fail)
        );
        // pending beats pass.
        let s = suites(&[Some("SUCCESS"), None]);
        assert_eq!(
            derive_status(Some("OPEN"), Some("MERGEABLE"), &s),
            Some(Status::Pending)
        );
        // all concluded, no failures -> pass.
        let s = suites(&[Some("SUCCESS"), Some("NEUTRAL")]);
        assert_eq!(
            derive_status(Some("OPEN"), Some("MERGEABLE"), &s),
            Some(Status::Pass)
        );
        // no check suites -> none.
        assert_eq!(derive_status(Some("OPEN"), Some("MERGEABLE"), &[]), None);
    }

    #[test]
    fn counts_only_failing_conclusions() {
        let s = suites(&[
            Some("SUCCESS"),
            Some("FAILURE"),
            Some("CANCELLED"),
            Some("STARTUP_FAILURE"),
            Some("TIMED_OUT"),
            Some("ACTION_REQUIRED"),
            None,
            Some("NEUTRAL"),
        ]);
        assert_eq!(fail_count(&s), 5);
    }

    #[test]
    fn timed_out_counts_as_failure() {
        let s = suites(&[Some("TIMED_OUT")]);
        assert_eq!(fail_count(&s), 1);
        assert_eq!(
            derive_status(Some("OPEN"), Some("MERGEABLE"), &s),
            Some(Status::Fail)
        );
    }

    #[test]
    fn phantom_zero_run_suites_are_ignored() {
        // A CLEAN, mergeable PR (github/copilot-agent-runtime#10703) whose only
        // "failures" are zero-run notify-pending-deployment suites, plus a pile
        // of never-running QUEUED app subscriptions, is green — not red/yellow.
        let s = vec![
            suite(Some("SUCCESS"), 22),
            suite(Some("SUCCESS"), 35),
            suite(Some("FAILURE"), 0), // phantom: notify-pending-deployment.yml
            suite(Some("FAILURE"), 0),
            suite(None, 0), // phantom: QUEUED app that never ran
            suite(None, 0),
        ];
        assert_eq!(fail_count(&s), 0);
        assert_eq!(
            derive_status(Some("OPEN"), Some("MERGEABLE"), &s),
            Some(Status::Pass)
        );
        // A real failing run (runs > 0) still counts.
        let s = vec![suite(Some("SUCCESS"), 3), suite(Some("FAILURE"), 1)];
        assert_eq!(fail_count(&s), 1);
        assert_eq!(
            derive_status(Some("OPEN"), Some("MERGEABLE"), &s),
            Some(Status::Fail)
        );
        // Only phantom suites -> no real CI -> none.
        let s = vec![suite(Some("FAILURE"), 0), suite(None, 0)];
        assert_eq!(derive_status(Some("OPEN"), Some("MERGEABLE"), &s), None);
    }

    #[test]
    fn zero_run_startup_failure_counts_as_failing() {
        // A genuine terminal failure: the suite failed to start, so it produced
        // zero runs. Unlike a zero-run FAILURE/CANCELLED phantom, it must not be
        // masked by the phantom filter — a broken pipeline can't read green.
        let s = vec![suite(Some("SUCCESS"), 4), suite(Some("STARTUP_FAILURE"), 0)];
        assert_eq!(fail_count(&s), 1);
        assert_eq!(
            derive_status(Some("OPEN"), Some("MERGEABLE"), &s),
            Some(Status::Fail)
        );
        // A lone zero-run STARTUP_FAILURE is still a failure, not "none".
        let s = vec![suite(Some("STARTUP_FAILURE"), 0)];
        assert_eq!(
            derive_status(Some("OPEN"), Some("MERGEABLE"), &s),
            Some(Status::Fail)
        );
    }

    #[test]
    fn state_styles_match_palette() {
        assert_eq!(state_style("CLEAN"), fg(GREEN));
        assert_eq!(state_style("UNSTABLE"), fg(YELLOW));
        assert_eq!(state_style("BLOCKED"), fg(YELLOW));
        assert_eq!(state_style("DIRTY"), fg(RED));
        assert_eq!(state_style("WHATEVER"), Style::new().dimmed());
    }

    #[test]
    fn dirty_is_labelled_conflicts() {
        assert_eq!(state_label("DIRTY"), "CONFLICTS");
        assert_eq!(state_label("CLEAN"), "CLEAN");
        assert_eq!(state_label("BLOCKED"), "BLOCKED");
    }

    #[test]
    fn state_glyphs_are_distinct_from_status_glyphs() {
        let states = [
            "CLEAN", "UNSTABLE", "BLOCKED", "BEHIND", "DIRTY", "DRAFT", "UNKNOWN",
        ];
        let status_glyphs: Vec<char> = ORDER.iter().map(|s| status_style(*s).0).collect();
        for st in states {
            let g = state_glyph(st);
            assert!(
                !status_glyphs.contains(&g),
                "state glyph for {st} collides with a status glyph"
            );
        }
        assert_eq!(state_glyph("CLEAN"), '\u{f00c}');
        assert_eq!(state_glyph("DIRTY"), '\u{f127}');
    }
}

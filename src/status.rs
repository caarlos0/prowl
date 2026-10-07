//! Shared status palette — the single source of truth for every glyph and color
//! the dashboard draws: PR approval, conflicts, the check-run semaphore, and
//! review state. Catppuccin Mocha colors, Nerd Font glyphs, 24-bit truecolor.

use uncurses::color::Color;
use uncurses::style::Style;

// Catppuccin Mocha palette (the subset the dashboard uses).
pub const GREEN: Color = Color::rgb(166, 227, 161); // #a6e3a1
pub const RED: Color = Color::rgb(243, 139, 168); // #f38ba8
pub const YELLOW: Color = Color::rgb(249, 226, 175); // #f9e2af
pub const MAUVE: Color = Color::rgb(203, 166, 247); // #cba6f7
pub const PEACH: Color = Color::rgb(250, 179, 135); // #fab387
pub const BLUE: Color = Color::rgb(137, 180, 250); // #89b4fa
pub const LAVENDER: Color = Color::rgb(180, 190, 254); // #b4befe
pub const PINK: Color = Color::rgb(245, 194, 231); // #f5c2e7 — unread change marker
pub const OVERLAY: Color = Color::rgb(147, 153, 178); // #9399b2 — muted accent (help legend)
pub const SURFACE: Color = Color::rgb(69, 71, 90); // #45475a — selected-row background

/// Coarse CI/merge state of an open PR. It is no longer rendered directly (the
/// row shows an approval glyph, a check-run semaphore, and a marked title when
/// it conflicts); it exists as the bell's change key, so a finishing job
/// doesn't ring but a red/green flip does.
#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Status {
    Conflicts,
    Fail,
    Pending,
    Pass,
}

/// A PR's approvals — the leading glyph of the "My open PRs" table.
/// Checks, unresolved threads, and conflicts each have their own place.
#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Approval {
    /// GitHub reports that all required reviews are approved.
    RequiredApproved,
    /// A reviewer approved it.
    Approved,
    /// Nobody has approved it yet.
    Pending,
}

/// Approval states in legend order.
pub const APPROVAL_ORDER: [Approval; 3] = [
    Approval::RequiredApproved,
    Approval::Approved,
    Approval::Pending,
];

/// GitHub's review decision determines whether required approvals are met.
/// Otherwise, keep showing any approval, including when no reviews are required
/// (`reviewDecision` is null) or another reviewer has requested changes.
pub fn approval_of<'a>(
    review_decision: Option<&str>,
    latest_reviews: impl IntoIterator<Item = &'a str>,
) -> Approval {
    if review_decision == Some("APPROVED") {
        Approval::RequiredApproved
    } else if latest_reviews.into_iter().any(|state| state == "APPROVED") {
        Approval::Approved
    } else {
        Approval::Pending
    }
}

/// Glyph + truecolor for an approval state.
pub fn approval_style(a: Approval) -> (char, Color) {
    match a {
        Approval::RequiredApproved => ('\u{ee29}', GREEN), // check-double
        Approval::Approved => ('\u{f00c}', GREEN),         // check
        Approval::Pending => ('\u{f05e}', YELLOW),         // ban
    }
}

/// ASCII fallback letter for an approval state.
pub fn approval_ascii(a: Approval) -> char {
    match a {
        Approval::RequiredApproved => 'Y',
        Approval::Approved => 'y',
        Approval::Pending => 'n',
    }
}

/// The approval glyph to render, honoring the ASCII toggle.
pub fn approval_glyph(a: Approval, ascii: bool) -> char {
    if ascii {
        approval_ascii(a)
    } else {
        approval_style(a).0
    }
}

/// One-line meaning of an approval state (for the help legend).
pub fn approval_meaning(a: Approval) -> &'static str {
    match a {
        Approval::RequiredApproved => "all required reviews approved",
        Approval::Approved => "a reviewer approved it",
        Approval::Pending => "nobody has approved it yet",
    }
}

/// Whether a PR conflicts with its base branch. GitHub computes `mergeable` and
/// `mergeStateStatus` in the same background job and one can land first, so a
/// conflict reported by either counts. Nothing else is collapsed in here: every
/// other reason a merge waits has its own column.
pub fn conflicts_of(merge_state: Option<&str>, mergeable: Option<&str>) -> bool {
    mergeable == Some("CONFLICTING") || merge_state == Some("DIRTY")
}

/// The marker that prefixes the title of a conflicting PR. It rides in the
/// title, not in a column of its own: conflicts are rare, and a column would
/// cost every row of every table three columns of width.
pub fn conflict_marker(ascii: bool) -> char {
    if ascii {
        '!'
    } else {
        '\u{f127}' // broken link
    }
}

/// One-line meaning of the conflict marker (for the help legend). It says where
/// the marker appears, since it is the only one that is not a column of its own.
pub const CONFLICT_MEANING: &str =
    "before a title: conflicts with the base branch \u{2014} needs a rebase";

/// A reviewer's relationship to a PR, for the Reviews view's per-row glyph.
/// Precedence when both apply: a pending request (re-review / awaiting) beats a
/// quiet "updated" or "reviewed".
#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReviewState {
    /// Review requested from you; you haven't reviewed yet.
    Awaiting,
    /// Re-requested after you already reviewed — the author wants another look.
    ReReview,
    /// You reviewed, and there are new commits since.
    Updated,
    /// You reviewed; nothing new and no pending request.
    Reviewed,
}

/// Review states in legend / sort order (most actionable first).
pub const REVIEW_ORDER: [ReviewState; 4] = [
    ReviewState::Awaiting,
    ReviewState::ReReview,
    ReviewState::Updated,
    ReviewState::Reviewed,
];

/// Glyph + truecolor for a review state — the Reviews view's per-row indicator.
pub fn review_style(s: ReviewState) -> (char, Color) {
    match s {
        ReviewState::Awaiting => ('\u{F06E}', YELLOW), // eye: needs your review
        ReviewState::ReReview => ('\u{F021}', PEACH),  // rotate: look again
        ReviewState::Updated => ('\u{F0AA}', BLUE),    // arrow-up: new commits
        ReviewState::Reviewed => ('\u{F00C}', GREEN),  // check: done
    }
}

/// ASCII fallback letter for a review state (non-Nerd-Font / piped output).
pub fn review_ascii(s: ReviewState) -> char {
    match s {
        ReviewState::Awaiting => 'a',
        ReviewState::ReReview => '@',
        ReviewState::Updated => '^',
        ReviewState::Reviewed => 'v',
    }
}

/// The review-state glyph to render, honoring the ASCII toggle.
pub fn review_glyph(s: ReviewState, ascii: bool) -> char {
    if ascii {
        review_ascii(s)
    } else {
        review_style(s).0
    }
}

/// One-line meaning of a review state (for the help legend).
pub fn review_meaning(s: ReviewState) -> &'static str {
    match s {
        ReviewState::Awaiting => "review requested from you",
        ReviewState::ReReview => "re-review requested after you reviewed",
        ReviewState::Updated => "updated since your review — new commits",
        ReviewState::Reviewed => "you've reviewed; nothing pending",
    }
}

/// A foreground style for a palette color.
pub fn fg(color: Color) -> Style {
    Style::new().fg(color)
}

/// The three lamps of the per-PR check semaphore.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Lamp {
    Fail,
    Running,
    Pass,
}

/// Truecolor for a lamp — red / yellow / green, straight from the palette.
pub fn lamp_color(l: Lamp) -> Color {
    match l {
        Lamp::Fail => RED,
        Lamp::Running => YELLOW,
        Lamp::Pass => GREEN,
    }
}

/// How many check runs sit on each lamp. Default counts come from GitHub's
/// rollup aggregates; required-only counts come from every paginated context.
/// For My PRs, the running count also includes unreported required checks.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Checks {
    pub fail: u64,
    pub running: u64,
    pub pass: u64,
}

impl Checks {
    /// Whether the PR reported no checks at all.
    pub fn is_empty(self) -> bool {
        self.fail == 0 && self.running == 0 && self.pass == 0
    }

    /// The count on one lamp.
    pub fn on(self, l: Lamp) -> u64 {
        match l {
            Lamp::Fail => self.fail,
            Lamp::Running => self.running,
            Lamp::Pass => self.pass,
        }
    }

    /// Add `count` runs to the lamp `l` maps to (a state we don't know about is
    /// simply not counted).
    pub fn add(&mut self, l: Option<Lamp>, count: u64) {
        match l {
            Some(Lamp::Fail) => self.fail += count,
            Some(Lamp::Running) => self.running += count,
            Some(Lamp::Pass) => self.pass += count,
            None => {}
        }
    }
}

/// Which lamp a `CheckRunState` lights up. `STALE` is deliberately unmapped:
/// it neither blocks a merge nor is still running.
pub fn check_run_lamp(state: &str) -> Option<Lamp> {
    match state {
        "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => {
            Some(Lamp::Fail)
        }
        "QUEUED" | "IN_PROGRESS" | "PENDING" | "WAITING" | "REQUESTED" => Some(Lamp::Running),
        "SUCCESS" | "NEUTRAL" | "SKIPPED" | "COMPLETED" => Some(Lamp::Pass),
        _ => None,
    }
}

/// Which lamp a legacy commit-status `StatusState` lights up.
pub fn status_context_lamp(state: &str) -> Option<Lamp> {
    match state {
        "ERROR" | "FAILURE" => Some(Lamp::Fail),
        "PENDING" | "EXPECTED" => Some(Lamp::Running),
        "SUCCESS" => Some(Lamp::Pass),
        _ => None,
    }
}

/// The coarse bell key for an open PR, with the precedence
/// `conflicts > fail > running > pass > none`.
pub fn derive_status(conflicts: bool, c: Checks) -> Option<Status> {
    if conflicts {
        return Some(Status::Conflicts);
    }
    if c.fail > 0 {
        return Some(Status::Fail);
    }
    if c.running > 0 {
        return Some(Status::Pending);
    }
    if c.pass > 0 {
        return Some(Status::Pass);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `Checks` from (fail, running, pass).
    fn checks(fail: u64, running: u64, pass: u64) -> Checks {
        Checks {
            fail,
            running,
            pass,
        }
    }

    #[test]
    fn conflicts_come_from_either_field() {
        // Both fields agree.
        assert!(conflicts_of(Some("DIRTY"), Some("CONFLICTING")));
        // They are computed by the same job and one can land first, so a
        // conflict reported by either one counts.
        assert!(conflicts_of(Some("UNKNOWN"), Some("CONFLICTING")));
        assert!(conflicts_of(Some("DIRTY"), None));
        // Everything that holds a merge for another reason is not a conflict:
        // those reasons have their own columns.
        for state in [
            "CLEAN",
            "BLOCKED",
            "BEHIND",
            "UNSTABLE",
            "DRAFT",
            "HAS_HOOKS",
        ] {
            assert!(!conflicts_of(Some(state), Some("MERGEABLE")), "{state}");
        }
        assert!(!conflicts_of(Some("UNKNOWN"), None));
        assert!(!conflicts_of(None, None));
    }

    #[test]
    fn the_conflict_marker_honors_the_ascii_toggle() {
        assert_eq!(conflict_marker(true), '!');
        assert_eq!(conflict_marker(false), '\u{f127}');
    }

    #[test]
    fn approval_palette_glyphs_colors_and_letters() {
        assert_eq!(
            approval_style(Approval::RequiredApproved),
            ('\u{ee29}', GREEN)
        );
        assert_eq!(approval_style(Approval::Approved), ('\u{f00c}', GREEN));
        assert_eq!(approval_style(Approval::Pending), ('\u{f05e}', YELLOW));
        assert_eq!(approval_ascii(Approval::RequiredApproved), 'Y');
        assert_eq!(approval_ascii(Approval::Approved), 'y');
        assert_eq!(approval_ascii(Approval::Pending), 'n');
        // The ASCII toggle picks the letter; otherwise the glyph.
        assert_eq!(approval_glyph(Approval::RequiredApproved, true), 'Y');
        assert_eq!(
            approval_glyph(Approval::RequiredApproved, false),
            '\u{ee29}'
        );
        assert_eq!(approval_glyph(Approval::Approved, true), 'y');
        assert_eq!(approval_glyph(Approval::Approved, false), '\u{f00c}');
    }

    #[test]
    fn one_approval_is_enough_and_a_change_request_does_not_undo_it() {
        assert_eq!(approval_of(None, []), Approval::Pending);
        assert_eq!(approval_of(None, ["APPROVED"]), Approval::Approved);
        assert_eq!(approval_of(None, ["CHANGES_REQUESTED"]), Approval::Pending);
        // A change request leaves the approval standing: the THREADS column
        // reports what is still open, so the glyph keeps its one meaning.
        assert_eq!(
            approval_of(None, ["APPROVED", "CHANGES_REQUESTED"]),
            Approval::Approved
        );
        assert_eq!(
            approval_of(None, ["CHANGES_REQUESTED", "APPROVED"]),
            Approval::Approved
        );
    }

    #[test]
    fn required_approval_uses_the_decision_not_the_review_page() {
        assert_eq!(
            approval_of(Some("APPROVED"), []),
            Approval::RequiredApproved
        );
        for decision in [None, Some("REVIEW_REQUIRED"), Some("CHANGES_REQUESTED")] {
            assert_eq!(
                approval_of(decision, ["APPROVED", "APPROVED"]),
                Approval::Approved,
                "{decision:?}"
            );
        }
    }

    #[test]
    fn review_letters_are_distinct_from_the_open_pr_letters() {
        let letters: Vec<char> = APPROVAL_ORDER
            .iter()
            .map(|a| approval_ascii(*a))
            .chain([conflict_marker(true)])
            .collect();
        for r in REVIEW_ORDER {
            assert!(
                !letters.contains(&review_ascii(r)),
                "review letter for {r:?} collides with an open-PR letter"
            );
        }
    }

    #[test]
    fn check_run_states_map_to_lamps() {
        for red in [
            "FAILURE",
            "TIMED_OUT",
            "CANCELLED",
            "ACTION_REQUIRED",
            "STARTUP_FAILURE",
        ] {
            assert_eq!(check_run_lamp(red), Some(Lamp::Fail), "{red}");
        }
        for yellow in ["QUEUED", "IN_PROGRESS", "PENDING", "WAITING", "REQUESTED"] {
            assert_eq!(check_run_lamp(yellow), Some(Lamp::Running), "{yellow}");
        }
        for green in ["SUCCESS", "NEUTRAL", "SKIPPED", "COMPLETED"] {
            assert_eq!(check_run_lamp(green), Some(Lamp::Pass), "{green}");
        }
        // STALE blocks nothing and isn't running, so it lights no lamp.
        assert_eq!(check_run_lamp("STALE"), None);
        assert_eq!(check_run_lamp("WHATEVER"), None);
    }

    #[test]
    fn status_context_states_map_to_lamps() {
        assert_eq!(status_context_lamp("ERROR"), Some(Lamp::Fail));
        assert_eq!(status_context_lamp("FAILURE"), Some(Lamp::Fail));
        assert_eq!(status_context_lamp("PENDING"), Some(Lamp::Running));
        assert_eq!(status_context_lamp("EXPECTED"), Some(Lamp::Running));
        assert_eq!(status_context_lamp("SUCCESS"), Some(Lamp::Pass));
        assert_eq!(status_context_lamp("WHATEVER"), None);
    }

    #[test]
    fn adding_counts_fills_the_right_lamp() {
        let mut c = Checks::default();
        assert!(c.is_empty());
        c.add(Some(Lamp::Fail), 2);
        c.add(Some(Lamp::Running), 3);
        c.add(Some(Lamp::Pass), 10);
        c.add(None, 99); // an unmapped state is dropped
        assert_eq!(c, checks(2, 3, 10));
        assert_eq!(c.on(Lamp::Fail), 2);
        assert_eq!(c.on(Lamp::Running), 3);
        assert_eq!(c.on(Lamp::Pass), 10);
        assert!(!c.is_empty());
    }

    #[test]
    fn lamp_colors_are_a_semaphore() {
        assert_eq!(lamp_color(Lamp::Fail), RED);
        assert_eq!(lamp_color(Lamp::Running), YELLOW);
        assert_eq!(lamp_color(Lamp::Pass), GREEN);
    }

    #[test]
    fn precedence_is_respected() {
        // Conflicts beat failing checks.
        assert_eq!(
            derive_status(true, checks(2, 1, 5)),
            Some(Status::Conflicts)
        );
        // Fail beats running.
        assert_eq!(derive_status(false, checks(1, 3, 5)), Some(Status::Fail));
        // Running beats pass.
        assert_eq!(derive_status(false, checks(0, 3, 5)), Some(Status::Pending));
        // All green.
        assert_eq!(derive_status(false, checks(0, 0, 5)), Some(Status::Pass));
        // No checks at all -> nothing to report.
        assert_eq!(derive_status(false, Checks::default()), None);
    }

    #[test]
    fn review_palette_glyphs_colors_and_letters() {
        assert_eq!(review_style(ReviewState::Awaiting), ('\u{F06E}', YELLOW));
        assert_eq!(review_style(ReviewState::ReReview), ('\u{F021}', PEACH));
        assert_eq!(review_style(ReviewState::Updated), ('\u{F0AA}', BLUE));
        assert_eq!(review_style(ReviewState::Reviewed), ('\u{F00C}', GREEN));
        assert_eq!(review_ascii(ReviewState::Awaiting), 'a');
        assert_eq!(review_ascii(ReviewState::ReReview), '@');
        assert_eq!(review_ascii(ReviewState::Updated), '^');
        assert_eq!(review_ascii(ReviewState::Reviewed), 'v');
        // The ASCII toggle picks the letter; otherwise the glyph.
        assert_eq!(review_glyph(ReviewState::Updated, true), '^');
        assert_eq!(review_glyph(ReviewState::Updated, false), '\u{F0AA}');
    }
}

//! Command-line interface.

use anyhow::{Result, bail};
use clap::{Parser, ValueEnum};
use std::str::FromStr;
use std::time::Duration;

/// The interactive watch-mode key bindings, appended to `--help`. They aren't
/// flags, so clap wouldn't list them otherwise; the running dashboard shows the
/// same keys in its footer and `?` legend.
const WATCH_KEYS: &str = "\
Keys (while watching):
  j / k            move the selection (also Down / Up arrows)
  g / G            jump to the first / last row
  Ctrl-D / Ctrl-U  move the selection half a page
  Enter            open the selected PR or release in your browser
  /                filter by number / title / author / release tag
  Esc              clear the filter
  r                refresh now
  Tab              switch view (your PRs / your reviews)
  ?                toggle the help legend
  Ctrl-C           quit";

#[derive(Parser, Debug)]
#[command(
    name = "prowl",
    version,
    about = "A tiny terminal radar for your GitHub pull requests.",
    after_help = WATCH_KEYS
)]
pub struct Cli {
    /// Repository to watch, as owner/name. Auto-detected from the cwd if omitted.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,

    /// Refresh interval, e.g. 30s, 10m, 2h.
    #[arg(long, default_value = "5m", value_name = "DUR", value_parser = parse_interval)]
    pub interval: Dur,

    /// Render once and exit (no watch loop, no bell).
    #[arg(long)]
    pub once: bool,

    /// Never ring the terminal bell on changes.
    #[arg(long)]
    pub no_bell: bool,

    /// Use ASCII status letters instead of Nerd Font glyphs (even on a TTY).
    #[arg(long)]
    pub ascii: bool,

    /// Sections to show, comma-separated. Default: all sections.
    #[arg(long, value_enum, value_delimiter = ',', value_name = "SECTION")]
    pub only: Option<Vec<Section>>,

    /// How far back "recently merged" reaches, e.g. 7d, 48h, 2w.
    #[arg(long, default_value = "2d", value_name = "DUR")]
    pub merged_window: Dur,

    /// Maximum number of recently-merged PRs to list.
    #[arg(long, default_value_t = 20, value_name = "N")]
    pub merged_limit: usize,

    /// Include pre-releases in the "My Shipments" section (they're skipped by default).
    #[arg(long)]
    pub include_pre_releases: bool,

    /// Which view to show first, toggled with Tab while watching: your PRs
    /// (open, queue, merged, shipments) or your code reviews.
    #[arg(long, value_enum, default_value_t = View::Mine, value_name = "VIEW")]
    pub view: View,

    /// In the Reviews view, whose requested reviews to include: only PRs that
    /// request you directly, or also those requesting a team you belong to.
    #[arg(long, value_enum, default_value_t = ReviewScope::All, value_name = "SCOPE")]
    pub review_scope: ReviewScope,

    /// Hide the help legend in one-shot/piped output (in the watch view it
    /// starts hidden and is toggled with `?`).
    #[arg(long)]
    pub no_help: bool,

    /// Authenticate with GitHub (device flow) and exit.
    #[arg(long)]
    pub login: bool,

    /// Don't read or write the on-disk cache (always start from a fresh fetch).
    #[arg(long)]
    pub no_cache: bool,

    /// Render a screen of synthetic demo data and exit (for screenshots).
    #[cfg(feature = "demo")]
    #[arg(long, hide = true)]
    pub demo: bool,
}

/// The dashboard sections, usable with `--only`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Section {
    Queue,
    Mine,
    Merged,
    Shipments,
}

/// The two dashboard views, toggled with Tab while watching.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum View {
    /// Your PRs: open, merge queue, merged, shipments.
    Mine,
    /// Code reviews: PRs to review, and merged PRs you reviewed.
    Reviews,
}

impl View {
    /// The other view (for the Tab toggle).
    pub fn toggle(self) -> View {
        match self {
            View::Mine => View::Reviews,
            View::Reviews => View::Mine,
        }
    }
}

/// Which requested reviews to include in the Reviews view.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum ReviewScope {
    /// Only PRs that request your review directly.
    Direct,
    /// PRs that request your review directly or via a team you belong to.
    All,
}

impl ReviewScope {
    /// The GitHub search qualifier for the "requesting my review" search.
    pub fn qualifier(self) -> &'static str {
        match self {
            ReviewScope::Direct => "user-review-requested",
            ReviewScope::All => "review-requested",
        }
    }
}

impl Cli {
    fn shows(&self, s: Section) -> bool {
        match &self.only {
            None => true,
            Some(list) => list.contains(&s),
        }
    }
    pub fn show_queue(&self) -> bool {
        self.shows(Section::Queue)
    }
    pub fn show_mine(&self) -> bool {
        self.shows(Section::Mine)
    }
    pub fn show_merged(&self) -> bool {
        self.shows(Section::Merged)
    }
    pub fn show_shipments(&self) -> bool {
        self.shows(Section::Shipments)
    }
}

/// A duration that remembers the string it was parsed from, so we can echo it
/// back (e.g. "the last 7d") instead of a normalized form.
#[derive(Clone, Debug)]
pub struct Dur {
    pub dur: Duration,
    pub raw: String,
}

impl FromStr for Dur {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Ok(Dur {
            dur: parse_duration(s).map_err(|e| e.to_string())?,
            raw: s.trim().to_string(),
        })
    }
}

/// Parse a `--interval` value, rejecting zero so the watch loop can't busy-spin
/// its fetches back-to-back.
fn parse_interval(s: &str) -> Result<Dur, String> {
    let dur = Dur::from_str(s)?;
    if dur.dur.is_zero() {
        return Err("interval must be greater than zero, e.g. 30s".to_string());
    }
    Ok(dur)
}

/// Parse a compact duration such as `30s`, `10m`, `2h`, `7d`, or `2w`.
/// A bare number is interpreted as seconds.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty duration");
    }
    let split = s
        .char_indices()
        .find(|(_, c)| c.is_ascii_alphabetic())
        .map_or(s.len(), |(i, _)| i);
    let (num, unit) = s.split_at(split);
    let n: u64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration `{s}`"))?;
    let factor: u64 = match unit {
        "" | "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" => 3600,
        "d" | "day" | "days" => 86_400,
        "w" | "wk" | "wks" => 604_800,
        other => bail!("invalid duration unit `{other}` (use s/m/h/d/w)"),
    };
    let secs = n
        .checked_mul(factor)
        .ok_or_else(|| anyhow::anyhow!("duration too large"))?;
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(604_800));
        assert_eq!(
            parse_duration("2w").unwrap(),
            Duration::from_secs(1_209_600)
        );
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
    }

    #[test]
    fn rejects_bad_durations() {
        for bad in ["", "abc", "10x", "m"] {
            assert!(parse_duration(bad).is_err(), "expected `{bad}` to fail");
        }
    }

    #[test]
    fn rejects_overflow() {
        assert!(parse_duration("99999999999999w").is_err());
    }

    #[test]
    fn rejects_zero_interval() {
        for zero in ["0", "0s", "0m"] {
            assert!(
                parse_interval(zero).is_err(),
                "expected interval `{zero}` to be rejected"
            );
        }
        assert!(parse_interval("30s").is_ok());
    }
}

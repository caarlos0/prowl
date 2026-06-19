//! Command-line interface.

use anyhow::{Result, bail};
use clap::{Parser, ValueEnum};
use std::str::FromStr;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(
    name = "prowl",
    version,
    about = "Watch a repo's recently merged PRs, merge queue, and your open PRs."
)]
pub struct Cli {
    /// Repository to watch, as owner/name. Auto-detected from the cwd if omitted.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,

    /// Refresh interval, e.g. 30s, 10m, 2h.
    #[arg(long, default_value = "5m", value_name = "DUR")]
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

    /// Sections to show, comma-separated. Default: all three.
    #[arg(long, value_enum, value_delimiter = ',', value_name = "SECTION")]
    pub only: Option<Vec<Section>>,

    /// How far back "recently merged" reaches, e.g. 7d, 48h, 2w.
    #[arg(long, default_value = "2d", value_name = "DUR")]
    pub merged_window: Dur,

    /// Maximum number of recently-merged PRs to list.
    #[arg(long, default_value_t = 20, value_name = "N")]
    pub merged_limit: usize,

    /// Hide the reference legend that explains the status glyphs and STATE values.
    #[arg(long)]
    pub no_reference: bool,
}

/// The dashboard sections, usable with `--only`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Section {
    Queue,
    Mine,
    Merged,
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
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration `{s}`"))?;
    let secs = match unit {
        "" | "s" | "sec" | "secs" => n,
        "m" | "min" | "mins" => n * 60,
        "h" | "hr" | "hrs" => n * 3600,
        "d" | "day" | "days" => n * 86_400,
        "w" | "wk" | "wks" => n * 604_800,
        other => bail!("invalid duration unit `{other}` (use s/m/h/d/w)"),
    };
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
        assert_eq!(parse_duration("2w").unwrap(), Duration::from_secs(1_209_600));
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
    }

    #[test]
    fn rejects_bad_durations() {
        for bad in ["", "abc", "10x", "m"] {
            assert!(parse_duration(bad).is_err(), "expected `{bad}` to fail");
        }
    }
}

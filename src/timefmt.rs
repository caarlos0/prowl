//! Small time helpers built on `chrono`: the `since` date for the merged
//! search, compact relative ages for merge times, and the local wall clock for
//! the status line.

use crate::cli::Dur;
use chrono::{DateTime, Duration as ChronoDuration, Local, Utc};
use std::time::Duration;

/// `YYYY-MM-DD` date `window` ago (UTC), for `merged:>=<since>`.
pub fn since_date(window: &Dur) -> String {
    let delta = ChronoDuration::from_std(window.dur).unwrap_or_else(|_| ChronoDuration::days(7));
    (Utc::now() - delta).format("%Y-%m-%d").to_string()
}

/// Local wall-clock `HH:MM:SS`, for the status line.
pub fn now_hms() -> String {
    Local::now().format("%H:%M:%S").to_string()
}

/// Compact ETA until the next refresh, `after` from now: seconds under a minute
/// (`45s`), otherwise minutes (`5m`), with hours rolled up for long intervals
/// (`2h`, `1h30m`). Pure (takes the interval) so it is trivially testable.
pub fn eta(after: Duration) -> String {
    let secs = after.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", (secs + 30) / 60)
    } else {
        let h = secs / 3_600;
        let m = (secs % 3_600 + 30) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m}m")
        }
    }
}

/// Parse an RFC 3339 timestamp (e.g. GitHub's `mergedAt`) to UTC.
pub fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Compact relative age of `then` as seen from `now`: `just now`, `5m`, `2h`,
/// `3d`, `1w`. Kept pure (both instants passed in) so it is trivially testable.
pub fn relative_age(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds().max(0);
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else if secs < 604_800 {
        format!("{}d", secs / 86_400)
    } else {
        format!("{}w", secs / 604_800)
    }
}

/// Relative age of an RFC 3339 string as seen from now, or `?` if unparseable.
pub fn age_of(ts: Option<&str>) -> String {
    match ts.and_then(parse_ts) {
        Some(t) => relative_age(Utc::now(), t),
        None => "?".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        parse_ts(s).unwrap()
    }

    #[test]
    fn eta_buckets_seconds_minutes_hours() {
        let s = |secs| eta(Duration::from_secs(secs));
        assert_eq!(s(30), "30s");
        assert_eq!(s(59), "59s");
        assert_eq!(s(60), "1m");
        assert_eq!(s(300), "5m");
        assert_eq!(s(90), "2m"); // rounds to nearest minute
        assert_eq!(s(3_600), "1h");
        assert_eq!(s(7_200), "2h");
        assert_eq!(s(5_400), "1h30m");
    }

    #[test]
    fn relative_age_buckets() {
        let now = at("2026-06-19T12:00:00Z");
        assert_eq!(relative_age(now, at("2026-06-19T11:59:30Z")), "just now");
        assert_eq!(relative_age(now, at("2026-06-19T11:55:00Z")), "5m");
        assert_eq!(relative_age(now, at("2026-06-19T10:00:00Z")), "2h");
        assert_eq!(relative_age(now, at("2026-06-16T12:00:00Z")), "3d");
        assert_eq!(relative_age(now, at("2026-06-12T12:00:00Z")), "1w");
        // Future timestamps clamp to "just now" rather than going negative.
        assert_eq!(relative_age(now, at("2026-06-19T12:05:00Z")), "just now");
    }
}

//! Saved CLI defaults, one `flag value` per line.

use anyhow::{Context, Result};
use clap::Command;
use std::path::{Path, PathBuf};

pub(crate) fn read() -> Result<Option<(PathBuf, String)>> {
    let base = if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty())
    {
        PathBuf::from(dir)
    } else if let Some(dir) = std::env::var_os("APPDATA").filter(|dir| !dir.is_empty()) {
        PathBuf::from(dir)
    } else if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        PathBuf::from(home).join(".config")
    } else {
        return Ok(None);
    };
    let path = base.join("prowl").join("config");
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some((path, text))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

pub(crate) fn apply(mut command: Command, path: &Path, text: &str) -> Result<Command> {
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        command = apply_line(command, line)
            .with_context(|| format!("{}:{}", path.display(), index + 1))?;
    }
    Ok(command)
}

fn apply_line(mut command: Command, line: &str) -> Result<Command> {
    let (name, value) = line
        .split_once(char::is_whitespace)
        .context("expected a flag name and value, e.g. bell false")?;
    let value = value.trim();
    let (name, value) = if name == "no-bell" {
        let disabled = value
            .parse::<bool>()
            .context("no-bell must be true or false")?;
        ("bell", (!disabled).to_string())
    } else {
        (name, value.to_string())
    };
    let arg = command
        .get_arguments()
        .find(|arg| arg.get_long() == Some(name))
        .with_context(|| format!("unknown config flag `{name}` (use long names without `--`)"))?;
    let id = arg.get_id().clone();
    command = command.mut_arg(id, |arg| arg.default_value(value));
    // Validate every saved value, even when the CLI will override it.
    command.clone().try_get_matches_from(["prowl"])?;
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, OpenSort, ReviewScope, Section, View};
    use clap::{CommandFactory, FromArgMatches, Parser};

    fn parse(text: &str, args: &[&str]) -> Cli {
        let command = apply(Cli::command(), Path::new("config"), text).unwrap();
        Cli::from_arg_matches(&command.try_get_matches_from(args).unwrap()).unwrap()
    }

    #[test]
    fn empty_config_keeps_cli_defaults() {
        let expected = Cli::parse_from(["prowl"]);
        let actual = parse("\n# defaults\n", &["prowl"]);
        assert_eq!(actual.interval.dur, expected.interval.dur);
        assert_eq!(actual.bell, expected.bell);
        assert_eq!(actual.branch, expected.branch);
        assert_eq!(actual.only, expected.only);
        assert_eq!(actual.view, expected.view);
        assert_eq!(actual.link_format, expected.link_format);
        assert_eq!(actual.sort_open, expected.sort_open);
    }

    #[test]
    fn values_use_cli_types_and_preserve_spaces() {
        let cli = parse(
            "# preferences\r\n\
             interval 30s\r\n\
             bell false\r\n\
             branch true\r\n\
             no-draft false\r\n\
             merged-limit 3\r\n\
             only mine,merged\r\n\
             view reviews\r\n\
             review-scope direct\r\n\
             link-format [{title}]({url}) # copied link\r\n",
            &["prowl"],
        );
        assert_eq!(cli.interval.dur.as_secs(), 30);
        assert!(!cli.bell);
        assert!(cli.branch);
        assert!(!cli.no_draft);
        assert_eq!(cli.merged_limit, 3);
        assert_eq!(cli.only, Some(vec![Section::Mine, Section::Merged]));
        assert_eq!(cli.view, View::Reviews);
        assert_eq!(cli.review_scope, ReviewScope::Direct);
        assert_eq!(cli.link_format, "[{title}]({url}) # copied link");
    }

    #[test]
    fn cli_overrides_saved_values_including_lists_and_bell() {
        let cli = parse(
            "interval 30s\nbell false\nonly mine,merged\nlink-format [{title}]({url})",
            &[
                "prowl",
                "--interval",
                "2m",
                "--bell=true",
                "--only",
                "queue",
                "--link-format",
                "{url}",
            ],
        );
        assert_eq!(cli.interval.dur.as_secs(), 120);
        assert!(cli.bell);
        assert_eq!(cli.only, Some(vec![Section::Queue]));
        assert_eq!(cli.link_format, "{url}");
        assert!(parse("bell true", &["prowl", "--no-bell"]).no_bell);
        assert!(!parse("bell true", &["prowl", "--bell=false"]).bell);
    }

    #[test]
    fn open_sort_uses_config_defaults_and_explicit_cli_overrides() {
        assert_eq!(
            parse("sort-open created", &["prowl"]).sort_open,
            OpenSort::Created
        );
        assert_eq!(
            parse("sort-open created", &["prowl", "--sort-open", "updated"]).sort_open,
            OpenSort::Updated
        );
        assert_eq!(
            parse("sort-open updated", &["prowl", "--sort-open", "created"]).sort_open,
            OpenSort::Created
        );
        assert_eq!(
            parse("sort-open created\nsort-open updated", &["prowl"]).sort_open,
            OpenSort::Updated
        );
    }

    #[test]
    fn whitespace_separates_names_but_not_words_in_values() {
        let cli = parse(
            "\tbranch\ttrue\n  link-format   {title} - {url}  \n",
            &["prowl"],
        );
        assert!(cli.branch);
        assert_eq!(cli.link_format, "{title} - {url}");
    }

    #[test]
    fn last_entry_wins_and_negative_bell_remains_compatible() {
        let cli = parse(
            "branch true\nbranch false\nbell false\nbell true",
            &["prowl"],
        );
        assert!(!cli.branch);
        assert!(cli.bell);
        assert!(!parse("no-bell true", &["prowl"]).bell);
        assert!(parse("no-bell false", &["prowl"]).bell);
        assert!(parse("no-bell true", &["prowl", "--bell=true"]).bell);
        assert!(parse("no-bell true\nbell true", &["prowl"]).bell);
        assert!(!parse("bell true\nno-bell true", &["prowl"]).bell);
    }

    #[test]
    fn invalid_lines_report_the_path_line_and_reason() {
        for (line, reason) in [
            ("bell", "expected a flag name and value"),
            ("bell   ", "expected a flag name and value"),
            ("bell maybe", "invalid value"),
            ("branch maybe", "invalid value"),
            ("no-bell maybe", "no-bell must be true or false"),
            ("interval 0s", "interval must be greater than zero"),
            ("merged-limit nope", "invalid value"),
            ("only unknown", "invalid value"),
            ("view unknown", "invalid value"),
            ("sort-open unknown", "invalid value"),
            ("unknown value", "unknown config flag"),
            ("--bell false", "unknown config flag"),
            ("help true", "unknown config flag"),
            ("version true", "unknown config flag"),
        ] {
            let error = apply(
                Cli::command(),
                Path::new("test/prowl/config"),
                &format!("# settings\n\n{line}"),
            )
            .unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains("test/prowl/config:3"),
                "{line:?}: {message}"
            );
            assert!(message.contains(reason), "{line:?}: {message}");
        }
    }
}

//! Open a URL in the user's default browser via the platform opener, without
//! pulling in a dependency. The opener is spawned detached (stdio nulled) and
//! never blocks or panics; a spawn failure is returned so the caller can show a
//! dim error line.

use std::process::{Command, Stdio};

/// The platform opener command and any fixed leading args it takes before the
/// URL: macOS `open`, Windows `rundll32`, everything else `xdg-open`.
fn opener() -> (&'static str, &'static [&'static str]) {
    if cfg!(target_os = "macos") {
        ("open", &[])
    } else if cfg!(target_os = "windows") {
        // Pass the URL to the native handler, not a command interpreter.
        ("rundll32", &["url.dll,FileProtocolHandler"])
    } else {
        ("xdg-open", &[])
    }
}

/// Open `url` in the default browser. Returns an error if the URL isn't a web
/// (`http`/`https`) URL or the opener could not be spawned; does not wait for it
/// to finish.
pub fn url(url: &str) -> std::io::Result<()> {
    command(url)?.spawn().map(|_| ())
}

fn command(url: &str) -> std::io::Result<Command> {
    // Defence in depth: only ever hand a web URL to the platform opener, so a
    // malformed target can't turn into a `file:` / `javascript:` open.
    // Every real target is a GitHub `https://` link.
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to open a non-web URL",
        ));
    }
    let (cmd, args) = opener();
    let mut command = Command::new(cmd);
    command
        .args(args)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opener_matches_platform() {
        let (cmd, args) = opener();
        if cfg!(target_os = "macos") {
            assert_eq!(cmd, "open");
            assert_eq!(args, Vec::<&str>::new());
        } else if cfg!(target_os = "windows") {
            assert_eq!(cmd, "rundll32");
            assert_eq!(args, &["url.dll,FileProtocolHandler"]);
        } else {
            assert_eq!(cmd, "xdg-open");
            assert_eq!(args, Vec::<&str>::new());
        }
    }

    #[test]
    fn rejects_non_web_urls() {
        // These fail the scheme check before any process is spawned.
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ftp://example.com/x",
            "not a url",
            "",
        ] {
            assert!(url(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn web_urls_are_passed_literally() {
        for target in [
            "https://github.com/example/repo/releases/tag/v2&ver&rem",
            "https://github.com/example/repo/compare/v1%PATH%...v2",
            "https://example.com/a path?value=\"quoted\"",
        ] {
            let command = command(target).unwrap();
            let (_, leading_args) = opener();
            let mut expected = leading_args
                .iter()
                .map(std::ffi::OsStr::new)
                .collect::<Vec<_>>();
            expected.push(std::ffi::OsStr::new(target));
            assert_eq!(command.get_args().collect::<Vec<_>>(), expected, "{target}");
        }
    }
}

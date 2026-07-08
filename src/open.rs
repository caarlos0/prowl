//! Open a URL in the user's default browser via the platform opener, without
//! pulling in a dependency. The opener is spawned detached (stdio nulled) and
//! never blocks or panics; a spawn failure is returned so the caller can show a
//! dim error line.

use std::process::{Command, Stdio};

/// The platform opener command and any fixed leading args it takes before the
/// URL: macOS `open`, Windows `cmd /C start ""`, everything else `xdg-open`.
fn opener() -> (&'static str, &'static [&'static str]) {
    if cfg!(target_os = "macos") {
        ("open", &[])
    } else if cfg!(target_os = "windows") {
        // `start` is a `cmd` builtin; the empty "" is its (ignored) window title,
        // which keeps a quoted URL from being swallowed as the title instead.
        ("cmd", &["/C", "start", ""])
    } else {
        ("xdg-open", &[])
    }
}

/// Open `url` in the default browser. Returns an error if the URL isn't a web
/// (`http`/`https`) URL or the opener could not be spawned; does not wait for it
/// to finish.
pub fn url(url: &str) -> std::io::Result<()> {
    // Defence in depth: only ever hand a web URL to the platform opener, so a
    // malformed target can't turn into a `file:` / `javascript:` open — or, on
    // Windows where `start` reparses its argument, a metacharacter injection.
    // Every real target is a GitHub `https://` link.
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to open a non-web URL",
        ));
    }
    let (cmd, args) = opener();
    Command::new(cmd)
        .args(args)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opener_matches_platform() {
        let (cmd, args) = opener();
        if cfg!(target_os = "macos") {
            assert_eq!(cmd, "open");
            assert!(args.is_empty());
        } else if cfg!(target_os = "windows") {
            assert_eq!(cmd, "cmd");
            assert_eq!(args, &["/C", "start", ""]);
        } else {
            assert_eq!(cmd, "xdg-open");
            assert!(args.is_empty());
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
}

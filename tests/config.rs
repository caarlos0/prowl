use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

struct ConfigDir(PathBuf);

impl ConfigDir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "prowl-config-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }

    fn write(&self, path: &str, text: impl AsRef<[u8]>) -> PathBuf {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_prowl"));
        command
            .env("XDG_CONFIG_HOME", &self.0)
            .env("HOME", &self.0)
            .env_remove("APPDATA")
            .env("PROWL_TOKEN", "config-test-token")
            // An invalid repo stops startup before any network request.
            .args(["--repo", "invalid-repo"]);
        command
    }
}

impl Drop for ConfigDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn failure(output: &Output, code: i32) -> String {
    assert_eq!(output.status.code(), Some(code), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    String::from_utf8(output.stderr.clone()).unwrap()
}

#[test]
fn missing_config_keeps_normal_startup() {
    let dir = ConfigDir::new();
    let output = dir.command().output().unwrap();
    let error = failure(&output, 1);
    assert!(error.contains("invalid repo `invalid-repo`"), "{error}");
}

#[test]
fn startup_uses_saved_defaults_and_cli_overrides() {
    let dir = ConfigDir::new();
    dir.write("prowl/config", "repo saved-repo\nbell false\n");
    let mut command = Command::new(env!("CARGO_BIN_EXE_prowl"));
    command
        .env("XDG_CONFIG_HOME", &dir.0)
        .env("PROWL_TOKEN", "config-test-token")
        // If config loading regresses, repo detection must fail without a fetch.
        .env("GIT_DIR", dir.0.join("no-repo"))
        .current_dir(&dir.0);
    let output = command.output().unwrap();
    let error = failure(&output, 1);
    assert!(error.contains("invalid repo `saved-repo`"), "{error}");

    let output = command.args(["--repo", "cli-repo"]).output().unwrap();
    let error = failure(&output, 1);
    assert!(error.contains("invalid repo `cli-repo`"), "{error}");
}

#[test]
fn config_errors_report_path_and_line_even_when_cli_overrides_the_value() {
    let dir = ConfigDir::new();
    for (setting, override_arg) in [
        ("bell maybe", "--bell=true"),
        ("sort-open maybe", "--sort-open=updated"),
    ] {
        let path = dir.write("prowl/config", format!("# settings\n{setting}\n"));
        let output = dir.command().arg(override_arg).output().unwrap();
        let error = failure(&output, 1);
        assert!(error.contains(&format!("{}:2", path.display())), "{error}");
        assert!(error.contains("invalid value 'maybe'"), "{error}");
    }
}

#[test]
fn help_version_and_cli_errors_do_not_read_config() {
    let dir = ConfigDir::new();
    std::fs::create_dir_all(dir.0.join("prowl/config")).unwrap();
    for arg in ["--help", "--version"] {
        let output = dir.command().arg(arg).output().unwrap();
        assert!(output.status.success(), "{arg}: {output:?}");
        assert!(output.stderr.is_empty(), "{arg}: {output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        if arg == "--help" {
            assert!(stdout.contains("--bell <BELL>"), "{stdout}");
            assert!(stdout.contains("--sort-open <ORDER>"), "{stdout}");
            assert!(stdout.contains("~/.config/prowl/config"), "{stdout}");
        } else {
            assert!(stdout.starts_with("prowl "), "{stdout}");
        }
    }
    for arg in ["--bell=maybe", "--sort-open=maybe"] {
        let output = dir.command().arg(arg).output().unwrap();
        let error = failure(&output, 2);
        assert!(error.contains("invalid value 'maybe'"), "{error}");
        assert!(!error.contains("reading "), "{error}");
    }
}

#[test]
fn unreadable_or_non_utf8_config_is_not_silently_ignored() {
    let dir = ConfigDir::new();
    let path = dir.0.join("prowl/config");
    std::fs::create_dir_all(&path).unwrap();
    let output = dir.command().output().unwrap();
    let error = failure(&output, 1);
    assert!(
        error.contains(&format!("reading {}", path.display())),
        "{error}"
    );

    std::fs::remove_dir(&path).unwrap();
    dir.write("prowl/config", [0xff]);
    let output = dir.command().output().unwrap();
    let error = failure(&output, 1);
    assert!(
        error.contains(&format!("reading {}", path.display())),
        "{error}"
    );
}

#[test]
fn config_path_uses_xdg_then_appdata_then_home() {
    let dir = ConfigDir::new();
    let xdg = dir.write("prowl/config", "from-xdg value");
    let appdata = dir.write("appdata/prowl/config", "from-appdata value");
    let home = dir.write(".config/prowl/config", "from-home value");
    for (xdg_dir, appdata_dir, expected) in [
        (Some(dir.0.clone()), Some(dir.0.join("appdata")), &xdg),
        (None, Some(dir.0.join("appdata")), &appdata),
        (None, None, &home),
        (Some(PathBuf::new()), Some(PathBuf::new()), &home),
    ] {
        let mut command = dir.command();
        command.env_remove("XDG_CONFIG_HOME");
        if let Some(path) = xdg_dir {
            command.env("XDG_CONFIG_HOME", path);
        }
        if let Some(path) = appdata_dir {
            command.env("APPDATA", path);
        }
        let output = command.output().unwrap();
        let error = failure(&output, 1);
        assert!(
            error.contains(&format!("{}:1", expected.display())),
            "{error}"
        );
    }
}

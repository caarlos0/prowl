//! GitHub OAuth device-flow login and token storage.
//!
//! The token is resolved from (in order): the `PROWL_TOKEN`/`GITHUB_TOKEN`
//! environment variables, the OS keyring (macOS/Windows) or a chmod-600 file
//! (Linux/headless), or — interactively — the OAuth device flow.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::time::{Duration, Instant};
use uncurses::style::Style;

/// prowl's GitHub OAuth App client id. Public by design (the device flow needs
/// no client secret); overridable for testing.
const CLIENT_ID: &str = "Ov23cttVkd9tfQwhdh3z";
const SCOPE: &str = "repo";
const KEYRING_SERVICE: &str = "prowl";
const KEYRING_ACCOUNT: &str = "github.com";

fn client_id() -> String {
    std::env::var("PROWL_CLIENT_ID").unwrap_or_else(|_| CLIENT_ID.to_string())
}

/// Resolve a token. `force_login` skips the cache and runs the device flow;
/// `interactive` allows the (blocking) device flow when no token is cached.
pub fn token(force_login: bool, interactive: bool) -> Result<String> {
    if !force_login {
        for var in ["PROWL_TOKEN", "GITHUB_TOKEN"] {
            if let Ok(t) = std::env::var(var)
                && !t.is_empty()
            {
                return Ok(t);
            }
        }
        if let Some(t) = load_stored() {
            return Ok(t);
        }
        // Auto-login only when there's a terminal to drive the device flow;
        // otherwise (piped/CI) point the user at --login or an env var.
        if !interactive {
            bail!("not authenticated: run `prowl --login`, or set GITHUB_TOKEN");
        }
    }
    let token = device_flow(interactive)?;
    store(&token);
    Ok(token)
}

// ---------------------------------------------------------------------------
// Device flow
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
}

fn device_flow(interactive: bool) -> Result<String> {
    let dc = request_device_code()?;
    // Always render the URL through a style; on an interactive terminal it also
    // carries an underlined, clickable OSC-8 hyperlink (emitted by the style).
    let mut style = Style::new();
    if interactive {
        style = style.underline().link(&dc.verification_uri, "");
    }
    eprintln!();
    eprintln!("  Authorize prowl:");
    eprintln!("    1. open {}", style.styled(&dc.verification_uri));
    eprintln!("    2. enter the code:  {}", dc.user_code);
    eprintln!();
    eprintln!("  Waiting for authorization...");
    poll_for_token(&dc)
}

fn request_device_code() -> Result<DeviceCode> {
    let id = client_id();
    let mut resp = ureq::post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .send_form([("client_id", id.as_str()), ("scope", SCOPE)])
        .context("requesting a device code")?;
    resp.body_mut()
        .read_json()
        .context("parsing the device-code response")
}

fn poll_for_token(dc: &DeviceCode) -> Result<String> {
    let id = client_id();
    let deadline = Instant::now() + Duration::from_secs(dc.expires_in);
    let mut interval = dc.interval.max(1);
    loop {
        std::thread::sleep(Duration::from_secs(interval));
        if Instant::now() >= deadline {
            bail!("the device code expired before authorization; try again");
        }
        let mut resp = ureq::post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .send_form([
                ("client_id", id.as_str()),
                ("device_code", dc.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .context("polling for the access token")?;
        let tr: TokenResponse = resp
            .body_mut()
            .read_json()
            .context("parsing the token response")?;
        if let Some(token) = tr.access_token {
            return Ok(token);
        }
        match tr.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some("expired_token") => bail!("the device code expired; try again"),
            Some("access_denied") => bail!("authorization was denied"),
            Some(other) => bail!("authorization failed: {other}"),
            None => bail!("authorization failed"),
        }
    }
}

// ---------------------------------------------------------------------------
// Storage: OS keyring (macOS/Windows) with a chmod-600 file fallback.
// ---------------------------------------------------------------------------

fn load_stored() -> Option<String> {
    if let Some(t) = keyring_get() {
        return Some(t);
    }
    file_get()
}

fn store(token: &str) {
    if keyring_set(token).is_ok() {
        return;
    }
    let _ = file_set(token);
}

fn keyring_entry() -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).context("opening keyring entry")
}

fn keyring_get() -> Option<String> {
    keyring_entry().ok()?.get_password().ok()
}

fn keyring_set(token: &str) -> Result<()> {
    keyring_entry()?
        .set_password(token)
        .context("writing to keyring")
}

fn token_file() -> Option<std::path::PathBuf> {
    let base = if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        std::path::PathBuf::from(dir)
    } else if let Ok(appdata) = std::env::var("APPDATA") {
        std::path::PathBuf::from(appdata)
    } else {
        std::path::PathBuf::from(std::env::var("HOME").ok()?).join(".config")
    };
    Some(base.join("prowl").join("token"))
}

fn file_get() -> Option<String> {
    let s = std::fs::read_to_string(token_file()?).ok()?;
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn file_set(token: &str) -> Result<()> {
    let path = token_file().context("no config directory")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("creating config directory")?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        // Create the file 0600 up front so the token is never world-readable.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .context("opening token file")?;
        f.write_all(token.as_bytes())
            .context("writing token file")?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, token).context("writing token file")?;
    Ok(())
}

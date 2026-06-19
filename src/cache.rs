//! On-disk cache of the last fetched data, so the watch dashboard can paint
//! instantly on startup (then refresh). Stored per repo under the user's cache
//! dir (`$XDG_CACHE_HOME/prowl`, `%LOCALAPPDATA%\prowl`, or `~/.cache/prowl`).

use crate::Sections;
use crate::github::Repo;
use crate::timefmt;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Bump when the cached layout changes; older files are then ignored.
const VERSION: u32 = 1;

/// A loaded cache entry.
#[derive(Deserialize)]
pub(crate) struct Cached {
    version: u32,
    pub(crate) me: String,
    pub(crate) saved_at: String,
    pub(crate) sections: Sections,
}

/// Borrowed view used to write the cache without cloning the sections.
#[derive(Serialize)]
struct CacheRef<'a> {
    version: u32,
    me: &'a str,
    saved_at: &'a str,
    sections: &'a Sections,
}

fn cache_dir() -> Option<PathBuf> {
    let base = if let Ok(d) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(d)
    } else if let Ok(d) = std::env::var("LOCALAPPDATA") {
        PathBuf::from(d)
    } else {
        PathBuf::from(std::env::var("HOME").ok()?).join(".cache")
    };
    Some(base.join("prowl"))
}

fn cache_file(repo: &Repo) -> Option<PathBuf> {
    Some(cache_dir()?.join(format!("{}_{}.json", repo.owner, repo.name)))
}

/// Load the cached sections for `repo`, if any (and matching the layout).
pub(crate) fn load(repo: &Repo) -> Option<Cached> {
    let bytes = std::fs::read(cache_file(repo)?).ok()?;
    let cached: Cached = serde_json::from_slice(&bytes).ok()?;
    (cached.version == VERSION).then_some(cached)
}

/// Write the current sections to the cache (best-effort; failures are ignored).
pub(crate) fn save(repo: &Repo, me: &str, sections: &Sections) {
    let Some(path) = cache_file(repo) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let saved_at = timefmt::now_hms();
    let data = CacheRef {
        version: VERSION,
        me,
        saved_at: &saved_at,
        sections,
    };
    if let Ok(bytes) = serde_json::to_vec(&data) {
        let _ = std::fs::write(path, bytes);
    }
}

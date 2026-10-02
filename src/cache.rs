//! On-disk cache of the last fetched data, so the watch dashboard can paint
//! instantly on startup (then refresh). Stored per repo under the user's cache
//! dir (`$XDG_CACHE_HOME/prowl`, `%LOCALAPPDATA%\prowl`, or `~/.cache/prowl`).

use crate::Sections;
use crate::cli::OpenSort;
use crate::github::Repo;
use crate::prs;
use crate::timefmt;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Bump when the cached data model changes; older files are then ignored.
const VERSION: u32 = 14;

/// A loaded cache entry.
#[derive(Deserialize)]
pub(crate) struct Cached {
    version: u32,
    required: bool,
    pub(crate) sections: Sections,
}

/// Borrowed view used to write the cache without cloning the sections.
#[derive(Serialize)]
struct CacheRef<'a> {
    version: u32,
    required: bool,
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
pub(crate) fn load(repo: &Repo, required: bool, sort_open: OpenSort) -> Option<Cached> {
    let bytes = std::fs::read(cache_file(repo)?).ok()?;
    parse(&bytes, required, sort_open)
}

fn parse(bytes: &[u8], required: bool, sort_open: OpenSort) -> Option<Cached> {
    let mut cached: Cached = serde_json::from_slice(bytes).ok()?;
    if !compatible(&cached, required) {
        return None;
    }
    if let Some(rows) = &mut cached.sections.prs {
        prs::sort_rows(rows, sort_open);
    }
    Some(cached)
}

fn compatible(cached: &Cached, required: bool) -> bool {
    cached.version == VERSION && cached.required == required
}

/// Write the current sections to the cache (best-effort; failures are ignored).
pub(crate) fn save(repo: &Repo, required: bool, sections: &Sections) {
    let Some(path) = cache_file(repo) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let saved_at = timefmt::now_hms();
    let data = CacheRef {
        version: VERSION,
        required,
        saved_at: &saved_at,
        sections,
    };
    let Ok(bytes) = serde_json::to_vec(&data) else {
        return;
    };
    if std::fs::write(&path, bytes).is_err() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_mode_must_match_required_flag() {
        let cached = Cached {
            version: VERSION,
            required: true,
            sections: Sections::EMPTY,
        };
        assert!(compatible(&cached, true));
        assert!(!compatible(&cached, false));
    }

    #[test]
    fn cached_open_prs_follow_the_current_sort_not_the_saved_order() {
        let mut data: crate::model::MineData =
            crate::github::parse_graphql(include_bytes!("../tests/fixtures/mine.json")).unwrap();
        for (node, created) in data.search.nodes.iter_mut().zip([
            "2026-01-01T00:00:00Z",
            "2026-01-03T00:00:00Z",
            "2026-01-02T00:00:00Z",
        ]) {
            node.created_at = Some(created.into());
        }
        let sections = Sections {
            prs: Some(prs::build_rows(data.search.nodes, OpenSort::Updated)),
            ..Sections::EMPTY
        };
        let encoded = serde_json::to_vec(&CacheRef {
            version: VERSION,
            required: false,
            saved_at: "12:00:00",
            sections: &sections,
        })
        .unwrap();

        let created = parse(&encoded, false, OpenSort::Created).unwrap();
        let rows = created.sections.prs.as_ref().unwrap();
        assert_eq!(
            rows.iter().map(|row| row.number).collect::<Vec<_>>(),
            [5323, 6656, 6475]
        );
        assert_eq!(rows[0].created_at.as_deref(), Some("2026-01-03T00:00:00Z"));
        let encoded = serde_json::to_vec(&CacheRef {
            version: VERSION,
            required: false,
            saved_at: "12:00:00",
            sections: &created.sections,
        })
        .unwrap();
        let updated = parse(&encoded, false, OpenSort::Updated).unwrap();
        assert_eq!(updated.sections.prs, sections.prs);
        assert!(parse(&encoded, true, OpenSort::Created).is_none());
    }

    #[test]
    fn cached_approval_states_round_trip() {
        let data: crate::model::MineData =
            crate::github::parse_graphql(include_bytes!("../tests/fixtures/mine.json")).unwrap();
        let mut rows = prs::build_rows(data.search.nodes, OpenSort::Updated);
        for (row, approval) in rows.iter_mut().zip(crate::status::APPROVAL_ORDER) {
            row.approval = approval;
        }
        let sections = Sections {
            prs: Some(rows),
            ..Sections::EMPTY
        };
        let encoded = serde_json::to_vec(&CacheRef {
            version: VERSION,
            required: false,
            saved_at: "12:00:00",
            sections: &sections,
        })
        .unwrap();
        let cached = parse(&encoded, false, OpenSort::Updated).unwrap();
        assert_eq!(cached.sections.prs, sections.prs);
    }

    #[test]
    fn older_cache_versions_are_invalidated() {
        let encoded = serde_json::to_vec(&CacheRef {
            version: VERSION - 1,
            required: false,
            saved_at: "12:00:00",
            sections: &Sections::EMPTY,
        })
        .unwrap();
        for sort in [OpenSort::Updated, OpenSort::Created] {
            assert!(parse(&encoded, false, sort).is_none());
        }
    }
}

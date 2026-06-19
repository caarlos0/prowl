//! "Commits by me" counts for the previous and next stable release of the
//! watched repo, via the GitHub releases + compare REST APIs.

use crate::gh::{self, Repo};
use anyhow::Result;
use serde::Deserialize;
use serde::de::DeserializeOwned;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitStats {
    /// False when the stats could not be computed (kept best-effort so a
    /// failure here never takes down the rest of the dashboard).
    pub available: bool,
    /// The most recent stable release tag, if any.
    pub previous_tag: Option<String>,
    /// My commits that shipped in the previous stable release.
    pub previous: Option<Count>,
    /// My commits since the previous stable release (the next release).
    pub next: Option<Count>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Count {
    pub mine: usize,
    /// True if the range exceeded the compare API's 250-commit window, so
    /// `mine` is a lower bound.
    pub capped: bool,
}

impl CommitStats {
    pub fn unavailable() -> CommitStats {
        CommitStats {
            available: false,
            previous_tag: None,
            previous: None,
            next: None,
        }
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
}

#[derive(Deserialize)]
struct Comparison {
    total_commits: usize,
    commits: Vec<CommitNode>,
}

#[derive(Deserialize)]
struct CommitNode {
    author: Option<Author>,
}

#[derive(Deserialize)]
struct Author {
    login: String,
}

fn api<T: DeserializeOwned>(path: &str) -> Result<T> {
    let bytes = gh::run(&["api", path])?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Stable (non-draft, non-prerelease) release tags, most recent first.
fn stable_tags(repo: &Repo) -> Result<Vec<String>> {
    let path = format!("repos/{}/{}/releases?per_page=50", repo.owner, repo.name);
    let releases: Vec<Release> = api(&path)?;
    Ok(releases
        .into_iter()
        .filter(|r| !r.draft && !r.prerelease)
        .map(|r| r.tag_name)
        .collect())
}

/// Count my commits among a comparison's commits (the compare API returns at
/// most 250).
fn count_mine(commits: &[CommitNode], total: usize, me: &str) -> Count {
    let mine = commits
        .iter()
        .filter(|c| c.author.as_ref().map(|a| a.login.as_str()) == Some(me))
        .count();
    Count {
        mine,
        capped: total > commits.len(),
    }
}

/// My commits in `base..head` via the compare API.
fn compare_mine(repo: &Repo, me: &str, base: &str, head: &str) -> Result<Count> {
    let path = format!(
        "repos/{}/{}/compare/{}...{}",
        repo.owner, repo.name, base, head
    );
    let cmp: Comparison = api(&path)?;
    Ok(count_mine(&cmp.commits, cmp.total_commits, me))
}

/// My commits reachable from `reff` (paginated, server-side author filter).
fn reachable_mine(repo: &Repo, me: &str, reff: &str, max_pages: usize) -> Result<Count> {
    let mut mine = 0usize;
    for page in 1..=max_pages {
        let path = format!(
            "repos/{}/{}/commits?sha={}&author={}&per_page=100&page={}",
            repo.owner, repo.name, reff, me, page
        );
        let nodes: Vec<CommitNode> = api(&path)?;
        let n = nodes.len();
        mine += n;
        if n < 100 {
            return Ok(Count {
                mine,
                capped: false,
            });
        }
    }
    Ok(Count { mine, capped: true })
}

/// Compute the commit stats for `repo`.
pub fn fetch(repo: &Repo, me: &str, default_branch: &str) -> Result<CommitStats> {
    let tags = stable_tags(repo)?;
    let latest = tags.first();
    let prev = tags.get(1);

    let (previous_tag, previous) = match (prev, latest) {
        (Some(p), Some(l)) => (Some(l.clone()), Some(compare_mine(repo, me, p, l)?)),
        // First release: count everything up to it.
        (None, Some(l)) => (Some(l.clone()), Some(reachable_mine(repo, me, l, 5)?)),
        _ => (None, None),
    };

    let next = match latest {
        Some(l) => Some(compare_mine(repo, me, l, default_branch)?),
        // No releases yet: everything on the default branch is the next release.
        None => Some(reachable_mine(repo, me, default_branch, 5)?),
    };

    Ok(CommitStats {
        available: true,
        previous_tag,
        previous,
        next,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes(logins: &[Option<&str>]) -> Vec<CommitNode> {
        logins
            .iter()
            .map(|l| CommitNode {
                author: l.map(|login| Author {
                    login: login.to_string(),
                }),
            })
            .collect()
    }

    #[test]
    fn counts_my_commits() {
        let c = nodes(&[Some("caarlos0"), Some("octocat"), None, Some("caarlos0")]);
        let got = count_mine(&c, 4, "caarlos0");
        assert_eq!(got.mine, 2);
        assert!(!got.capped);
    }

    #[test]
    fn flags_capped_ranges() {
        let c = nodes(&[Some("caarlos0")]);
        // total exceeds what the compare API returned.
        let got = count_mine(&c, 300, "caarlos0");
        assert_eq!(got.mine, 1);
        assert!(got.capped);
    }
}

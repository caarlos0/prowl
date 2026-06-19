//! "Commits by me" counts for the next (unreleased) version and the last few
//! stable releases of the watched repo, via the GitHub releases + compare REST
//! APIs.

use crate::github::{Client, Repo};
use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommitStats {
    /// False when the stats could not be computed (kept best-effort so a
    /// failure here never takes down the rest of the dashboard).
    pub available: bool,
    /// My commits heading into the next, still-unreleased version.
    pub upcoming: Option<Count>,
    /// The most recent stable releases (newest first) with my commit counts.
    pub releases: Vec<ReleaseCount>,
}

/// My commit count for a single shipped release.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReleaseCount {
    pub tag: String,
    pub count: Count,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
            upcoming: None,
            releases: Vec::new(),
        }
    }
}

/// How many recent stable releases to show commit counts for.
const RELEASES: usize = 4;

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

/// Stable (non-draft, non-prerelease) release tags, most recent first.
fn stable_tags(client: &Client, repo: &Repo) -> Result<Vec<String>> {
    let path = format!("repos/{}/{}/releases?per_page=50", repo.owner, repo.name);
    let releases: Vec<Release> = client.get(&path)?;
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
fn compare_mine(client: &Client, repo: &Repo, me: &str, base: &str, head: &str) -> Result<Count> {
    let path = format!(
        "repos/{}/{}/compare/{}...{}",
        repo.owner, repo.name, base, head
    );
    let cmp: Comparison = client.get(&path)?;
    Ok(count_mine(&cmp.commits, cmp.total_commits, me))
}

/// My commits reachable from `reff` (paginated, server-side author filter).
fn reachable_mine(
    client: &Client,
    repo: &Repo,
    me: &str,
    reff: &str,
    max_pages: usize,
) -> Result<Count> {
    let mut mine = 0usize;
    for page in 1..=max_pages {
        let path = format!(
            "repos/{}/{}/commits?sha={}&author={}&per_page=100&page={}",
            repo.owner, repo.name, reff, me, page
        );
        let nodes: Vec<CommitNode> = client.get(&path)?;
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

/// Compute the commit stats for `repo`: my commits in the next (unreleased)
/// version, plus my commits in each of the last [`RELEASES`] stable releases.
pub fn fetch(client: &Client, repo: &Repo, me: &str, default_branch: &str) -> Result<CommitStats> {
    let tags = stable_tags(client, repo)?;

    // The next release is everything since the latest tag (or the whole default
    // branch when there are no releases yet).
    let upcoming = match tags.first() {
        Some(latest) => Some(compare_mine(client, repo, me, latest, default_branch)?),
        None => Some(reachable_mine(client, repo, me, default_branch, 5)?),
    };

    // Each shipped release is the range between it and the release before it;
    // the oldest tag we know of has no predecessor, so count everything up to it.
    let mut releases = Vec::with_capacity(tags.len().min(RELEASES));
    for (i, tag) in tags.iter().enumerate().take(RELEASES) {
        let count = match tags.get(i + 1) {
            Some(base) => compare_mine(client, repo, me, base, tag)?,
            None => reachable_mine(client, repo, me, tag, 5)?,
        };
        releases.push(ReleaseCount {
            tag: tag.clone(),
            count,
        });
    }

    Ok(CommitStats {
        available: true,
        upcoming,
        releases,
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

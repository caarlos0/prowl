//! "Commits by me" counts for the next (unreleased) version and the last few
//! stable releases of the watched repo, via the GitHub releases + compare REST
//! APIs. `fetch` also returns a map from PR number to the release that shipped
//! it, used to annotate the recently-merged section.

use crate::github::{Client, Repo};
use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommitStats {
    /// False when the stats could not be computed (kept best-effort so a
    /// failure here never takes down the rest of the dashboard).
    pub available: bool,
    /// My work heading into the next, still-unreleased version.
    pub upcoming: Option<Bucket>,
    /// The most recent stable releases (newest first) with my work in them.
    pub releases: Vec<Release>,
}

/// My commit count for one version (upcoming or released), plus the link target
/// for its label: the compare log against the default branch (upcoming) or the
/// release page (a shipped release).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Bucket {
    pub count: Count,
    pub url: String,
}

/// My work in a single shipped release.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Release {
    pub tag: String,
    pub bucket: Bucket,
    /// When the release was published (RFC 3339), if known. Stored raw so its
    /// relative age stays fresh across redraws.
    pub published_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Count {
    pub mine: usize,
    /// True if the range exceeded the compare API's 250-commit window, so
    /// `mine` is a lower bound.
    pub capped: bool,
}

/// The release that shipped a PR: its tag and release-page URL.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReleaseRef {
    pub tag: String,
    pub url: String,
}

/// Maps a PR number to the (earliest) release that includes it. PRs merged
/// since the latest release are absent (not yet shipped).
pub type ReleaseMap = HashMap<i64, ReleaseRef>;

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

/// Page cap (×100 commits) when counting commits reachable from a ref with no
/// older release to compare against.
const MAX_PAGES: usize = 5;

#[derive(Deserialize)]
struct ReleaseInfo {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    published_at: Option<String>,
}

/// A shown release tag and when it was published.
struct Tag {
    name: String,
    published_at: Option<String>,
}

#[derive(Deserialize)]
struct Comparison {
    total_commits: usize,
    commits: Vec<CommitNode>,
}

#[derive(Deserialize)]
struct CommitNode {
    author: Option<Author>,
    commit: CommitMeta,
}

#[derive(Deserialize)]
struct CommitMeta {
    message: String,
}

#[derive(Deserialize)]
struct Author {
    login: String,
}

/// Shown release tags, most recent first, with their publish dates.
///
/// Drafts are always skipped. Prereleases are skipped unless
/// `include_prereleases` is set.
///
/// Releases are paginated until enough matching tags are found (we need one more
/// than [`RELEASES`] to have a compare base for the oldest shown release), or
/// the pages run out. Without this, a run of skipped releases (common during
/// goreleaser `-rc.N`) could fill the first page and hide every matching tag.
fn release_tags(client: &Client, repo: &Repo, include_prereleases: bool) -> Result<Vec<Tag>> {
    let want = RELEASES + 1;
    let mut tags = Vec::new();
    // Bounded so a repo with thousands of skipped releases can't loop forever.
    for page in 1..=20 {
        let path = format!(
            "repos/{}/{}/releases?per_page=50&page={page}",
            repo.owner, repo.name
        );
        let releases: Vec<ReleaseInfo> = client.get(&path)?;
        let exhausted = releases.len() < 50;
        tags.extend(
            releases
                .into_iter()
                .filter(|r| !r.draft && (include_prereleases || !r.prerelease))
                .map(|r| Tag {
                    name: r.tag_name,
                    published_at: r.published_at,
                }),
        );
        if tags.len() >= want || exhausted {
            break;
        }
    }
    Ok(tags)
}

/// The trailing `(#NNN)` PR number in a commit subject (the squash / merge
/// convention), or `None`. The reference must be at the very end and numeric.
fn pr_number(subject: &str) -> Option<u32> {
    let s = subject.trim_end();
    let inner = s.strip_suffix(')')?;
    let at = inner.rfind("(#")?;
    inner[at + 2..].parse().ok()
}

/// First-line PR number of a commit, if any.
fn commit_pr(node: &CommitNode) -> Option<u32> {
    pr_number(node.commit.message.lines().next().unwrap_or_default())
}

/// My commit count and the PR numbers I authored among `commits` (`total` is
/// the range's full size, so the count is flagged capped beyond what we fetched).
fn mine_in(commits: &[CommitNode], total: usize, me: &str) -> (Count, Vec<u32>) {
    let mut mine = 0usize;
    let mut prs = Vec::new();
    for c in commits {
        if c.author.as_ref().map(|a| a.login.as_str()) == Some(me) {
            mine += 1;
            if let Some(n) = commit_pr(c) {
                prs.push(n);
            }
        }
    }
    let count = Count {
        mine,
        capped: total > commits.len(),
    };
    (count, prs)
}

/// My commits in `base..head` via the compare API (at most 250 commits).
fn compare_mine(
    client: &Client,
    repo: &Repo,
    me: &str,
    base: &str,
    head: &str,
) -> Result<(Count, Vec<u32>)> {
    let path = format!(
        "repos/{}/{}/compare/{}...{}",
        repo.owner, repo.name, base, head
    );
    let cmp: Comparison = client.get(&path)?;
    Ok(mine_in(&cmp.commits, cmp.total_commits, me))
}

/// My commits reachable from `reff` (paginated, with a server-side author
/// filter), bounded to `MAX_PAGES` of 100 commits.
fn reachable_mine(client: &Client, repo: &Repo, me: &str, reff: &str) -> Result<(Count, Vec<u32>)> {
    let mut mine = 0usize;
    let mut prs = Vec::new();
    for page in 1..=MAX_PAGES {
        let path = format!(
            "repos/{}/{}/commits?sha={}&author={}&per_page=100&page={}",
            repo.owner, repo.name, reff, me, page
        );
        let nodes: Vec<CommitNode> = client.get(&path)?;
        let n = nodes.len();
        mine += n;
        prs.extend(nodes.iter().filter_map(commit_pr));
        if n < 100 {
            return Ok((
                Count {
                    mine,
                    capped: false,
                },
                prs,
            ));
        }
    }
    Ok((Count { mine, capped: true }, prs))
}

/// My commits in a version range: `base..head` via the compare API, or
/// everything reachable from `head` when there is no older base.
fn range_mine(
    client: &Client,
    repo: &Repo,
    me: &str,
    base: Option<&str>,
    head: &str,
) -> Result<(Count, Vec<u32>)> {
    match base {
        Some(base) => compare_mine(client, repo, me, base, head),
        None => reachable_mine(client, repo, me, head),
    }
}

/// Compute the commit stats for `repo` (my work in the next unreleased version
/// and each of the last [`RELEASES`] releases — stable only, or including
/// prereleases when `include_prereleases` is set), plus a map from PR number to
/// the release that shipped it (for annotating the merged section). PRs merged
/// since the latest release are absent from the map (not yet shipped).
pub fn fetch(
    client: &Client,
    repo: &Repo,
    me: &str,
    default_branch: &str,
    include_prereleases: bool,
) -> Result<(CommitStats, ReleaseMap)> {
    let tags = release_tags(client, repo, include_prereleases)?;
    let base_url = format!("https://github.com/{}/{}", repo.owner, repo.name);
    let mut map = ReleaseMap::new();

    // The next release is everything since the latest tag (or the whole default
    // branch when there are no releases yet); its label links to that log. Its
    // PRs are unreleased, so they are not added to the map.
    let latest = tags.first().map(|t| t.name.as_str());
    let (count, _) = range_mine(client, repo, me, latest, default_branch)?;
    let upcoming = Some(Bucket {
        count,
        url: match latest {
            Some(latest) => format!("{base_url}/compare/{latest}...{default_branch}"),
            None => format!("{base_url}/commits/{default_branch}"),
        },
    });

    // Each shipped release is the range between it and the release before it;
    // the oldest tag we know of has no predecessor, so count everything up to it.
    let mut releases = Vec::with_capacity(tags.len().min(RELEASES));
    for (i, tag) in tags.iter().enumerate().take(RELEASES) {
        let base = tags.get(i + 1).map(|t| t.name.as_str());
        let (count, prs) = range_mine(client, repo, me, base, &tag.name)?;
        let url = format!("{base_url}/releases/tag/{}", tag.name);
        // Newest release first, and ranges are disjoint, so a PR maps to the
        // earliest release that ships it.
        for n in prs {
            map.entry(i64::from(n)).or_insert_with(|| ReleaseRef {
                tag: tag.name.clone(),
                url: url.clone(),
            });
        }
        releases.push(Release {
            tag: tag.name.clone(),
            bucket: Bucket { count, url },
            published_at: tag.published_at.clone(),
        });
    }

    Ok((
        CommitStats {
            available: true,
            upcoming,
            releases,
        },
        map,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(login: Option<&str>, message: &str) -> CommitNode {
        CommitNode {
            author: login.map(|login| Author {
                login: login.to_string(),
            }),
            commit: CommitMeta {
                message: message.to_string(),
            },
        }
    }

    #[test]
    fn parses_trailing_pr_number() {
        assert_eq!(pr_number("feat: thing (#12)"), Some(12));
        assert_eq!(pr_number("chore: bump  (#7)\n"), Some(7));
        assert_eq!(pr_number("no reference here"), None);
        // A reference must be at the very end, and must be numeric.
        assert_eq!(pr_number("mid (#3) ref"), None);
        assert_eq!(pr_number("oops (#x)"), None);
    }

    #[test]
    fn counts_my_commits_and_their_prs() {
        let commits = vec![
            node(Some("caarlos0"), "feat: a (#10)"),
            node(Some("octocat"), "feat: b (#11)"),
            node(None, "direct commit"),
            node(Some("caarlos0"), "fix: c (#12)\n\nbody"),
        ];
        let (count, prs) = mine_in(&commits, 4, "caarlos0");
        assert_eq!(count.mine, 2);
        assert!(!count.capped);
        assert_eq!(prs, vec![10, 12]);
    }

    #[test]
    fn flags_capped_when_range_truncated() {
        let (count, _) = mine_in(&[node(Some("caarlos0"), "x (#1)")], 300, "caarlos0");
        assert!(count.capped);
    }
}

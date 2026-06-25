//! "Commits by me" counts for the next (unreleased) version and the last few
//! stable releases of the watched repo, via the GitHub releases + compare REST
//! APIs. Each bucket also lists the PRs I shipped that are still in the recent
//! merged set (so the dashboard cross-references where my recent merges landed).

use crate::github::{Client, Repo};
use anyhow::Result;
use serde::Deserialize;
use std::collections::HashSet;

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

/// My commit count and the PRs I shipped in one version (upcoming or released).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Bucket {
    pub count: Count,
    /// Link target for the bucket's label: the compare log against the default
    /// branch (upcoming) or the release page (a shipped release).
    pub url: String,
    pub prs: Vec<Shipped>,
}

/// One PR I shipped, identified by the trailing `(#NNN)` in its (squash /
/// merge) commit subject. Direct commits without a PR reference are not listed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Shipped {
    pub number: u32,
    pub url: String,
    pub title: String,
}

/// My work in a single shipped release.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Release {
    pub tag: String,
    pub bucket: Bucket,
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

/// Page cap (×100 commits) when counting commits reachable from a ref with no
/// older release to compare against.
const MAX_PAGES: usize = 5;

#[derive(Deserialize)]
struct ReleaseInfo {
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

/// Release tags, most recent first.
///
/// Drafts are always skipped. Prereleases are skipped unless
/// `include_prereleases` is set.
///
/// Releases are paginated until enough matching tags are found (we need one more
/// than [`RELEASES`] to have a compare base for the oldest shown release), or
/// the pages run out. Without this, a run of skipped releases (common during
/// goreleaser `-rc.N`) could fill the first page and hide every matching tag.
fn release_tags(client: &Client, repo: &Repo, include_prereleases: bool) -> Result<Vec<String>> {
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
                .map(|r| r.tag_name),
        );
        if tags.len() >= want || exhausted {
            break;
        }
    }
    Ok(tags)
}

/// Split a commit subject into its title and trailing `(#NNN)` PR number, if
/// any: `"feat: thing (#12)"` -> `("feat: thing", Some(12))`. The reference
/// must be at the very end, matching how squash / merge commits are titled.
fn split_pr_ref(subject: &str) -> (&str, Option<u32>) {
    let subject = subject.trim_end();
    let parsed = subject
        .strip_suffix(')')
        .and_then(|s| s.rfind("(#").map(|i| (&s[..i], &s[i + 2..])))
        .and_then(|(head, digits)| digits.parse::<u32>().ok().map(|n| (head.trim_end(), n)));
    match parsed {
        Some((head, n)) => (head, Some(n)),
        None => (subject, None),
    }
}

/// Turn a commit into a [`Shipped`] entry, or `None` for a direct commit with
/// no trailing `(#NNN)` PR reference.
fn to_shipped(node: &CommitNode, repo: &Repo) -> Option<Shipped> {
    let subject = node.commit.message.lines().next().unwrap_or_default();
    let (title, number) = split_pr_ref(subject);
    let number = number?;
    Some(Shipped {
        number,
        url: format!(
            "https://github.com/{}/{}/pull/{number}",
            repo.owner, repo.name
        ),
        title: title.to_string(),
    })
}

/// My commit count and shipped PRs in `base..head` via the compare API (which
/// returns at most 250 commits, so the count is flagged capped beyond that).
fn compare_mine(
    client: &Client,
    repo: &Repo,
    me: &str,
    base: &str,
    head: &str,
) -> Result<(Count, Vec<Shipped>)> {
    let path = format!(
        "repos/{}/{}/compare/{}...{}",
        repo.owner, repo.name, base, head
    );
    let cmp: Comparison = client.get(&path)?;
    let mine: Vec<&CommitNode> = cmp
        .commits
        .iter()
        .filter(|c| c.author.as_ref().map(|a| a.login.as_str()) == Some(me))
        .collect();
    let count = Count {
        mine: mine.len(),
        capped: cmp.total_commits > cmp.commits.len(),
    };
    let prs = mine
        .into_iter()
        .filter_map(|c| to_shipped(c, repo))
        .collect();
    Ok((count, prs))
}

/// My commit count and shipped PRs reachable from `reff` (paginated, with a
/// server-side author filter), bounded to `MAX_PAGES` of 100 commits.
fn reachable_mine(
    client: &Client,
    repo: &Repo,
    me: &str,
    reff: &str,
) -> Result<(Count, Vec<Shipped>)> {
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
        prs.extend(nodes.iter().filter_map(|c| to_shipped(c, repo)));
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

/// My commit count and recently-merged PRs for one version range: `base..head`
/// via the compare API, or everything reachable from `head` when there's no
/// base. PRs are filtered to those whose number is in `recent`.
fn mine_recent(
    client: &Client,
    repo: &Repo,
    me: &str,
    base: Option<&str>,
    head: &str,
    recent: &HashSet<i64>,
) -> Result<(Count, Vec<Shipped>)> {
    let (count, mut prs) = match base {
        Some(base) => compare_mine(client, repo, me, base, head)?,
        None => reachable_mine(client, repo, me, head)?,
    };
    prs.retain(|s| recent.contains(&i64::from(s.number)));
    Ok((count, prs))
}

/// Compute the commit stats for `repo`: my work in the next (unreleased)
/// version, plus my work in each of the last [`RELEASES`] releases (stable
/// only, or including prereleases when `include_prereleases` is set). Each
/// bucket lists only the PRs whose number is in `recent` (the recently-merged
/// set), so the section cross-references where my recent merges landed.
pub fn fetch(
    client: &Client,
    repo: &Repo,
    me: &str,
    default_branch: &str,
    include_prereleases: bool,
    recent: &HashSet<i64>,
) -> Result<CommitStats> {
    let tags = release_tags(client, repo, include_prereleases)?;
    let base_url = format!("https://github.com/{}/{}", repo.owner, repo.name);

    // The next release is everything since the latest tag (or the whole default
    // branch when there are no releases yet); its label links to that log.
    let latest = tags.first();
    let (count, prs) = mine_recent(
        client,
        repo,
        me,
        latest.map(String::as_str),
        default_branch,
        recent,
    )?;
    let upcoming = Some(Bucket {
        count,
        url: match latest {
            Some(latest) => format!("{base_url}/compare/{latest}...{default_branch}"),
            None => format!("{base_url}/commits/{default_branch}"),
        },
        prs,
    });

    // Each shipped release is the range between it and the release before it;
    // the oldest tag we know of has no predecessor, so count everything up to it.
    let mut releases = Vec::with_capacity(tags.len().min(RELEASES));
    for (i, tag) in tags.iter().enumerate().take(RELEASES) {
        let base = tags.get(i + 1).map(String::as_str);
        let (count, prs) = mine_recent(client, repo, me, base, tag, recent)?;
        releases.push(Release {
            tag: tag.clone(),
            bucket: Bucket {
                count,
                url: format!("{base_url}/releases/tag/{tag}"),
                prs,
            },
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

    fn repo() -> Repo {
        Repo {
            owner: "caarlos0".to_string(),
            name: "prowl".to_string(),
        }
    }

    #[test]
    fn parses_trailing_pr_reference() {
        assert_eq!(split_pr_ref("feat: thing (#12)"), ("feat: thing", Some(12)));
        assert_eq!(
            split_pr_ref("chore: bump  (#7)\n"),
            ("chore: bump", Some(7))
        );
        assert_eq!(
            split_pr_ref("no reference here"),
            ("no reference here", None)
        );
        // A reference must be at the very end, and must be numeric.
        assert_eq!(split_pr_ref("mid (#3) ref"), ("mid (#3) ref", None));
        assert_eq!(split_pr_ref("oops (#x)"), ("oops (#x)", None));
    }

    #[test]
    fn shipped_links_pr_and_strips_suffix() {
        let s = to_shipped(&node(Some("caarlos0"), "feat: x (#7)\n\nbody"), &repo()).unwrap();
        assert_eq!(s.title, "feat: x");
        assert_eq!(s.number, 7);
        assert_eq!(s.url, "https://github.com/caarlos0/prowl/pull/7");
    }

    #[test]
    fn direct_commit_is_not_shipped() {
        assert!(to_shipped(&node(None, "direct commit\n\nbody"), &repo()).is_none());
    }
}

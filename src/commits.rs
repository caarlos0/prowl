//! "Commits by me" counts for the next (unreleased) version and the last few
//! stable releases of the watched repo, via the GitHub releases + compare REST
//! APIs. `fetch` also returns a map from PR number to the release that shipped
//! it, used to annotate the recently-merged section.

use crate::github::{Client, Repo};
use anyhow::Result;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommitStats {
    /// False when the stats could not be computed (kept best-effort so a
    /// failure here never takes down the rest of the dashboard).
    pub available: bool,
    /// My work heading into the next, still-unreleased version; `None` when
    /// it was not counted.
    pub upcoming: Option<Bucket>,
    /// The most recent stable releases (newest first), with unavailable counts
    /// left as `None`.
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

/// A shipped release and its commit count when the range was read successfully.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Release {
    pub tag: String,
    pub count: Option<Count>,
    pub url: String,
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

/// Maps a PR number to the (earliest) release that includes it, tracking
/// whether every relevant release range was read successfully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseMap {
    releases: HashMap<i64, ReleaseRef>,
    complete: bool,
}

impl ReleaseMap {
    pub fn new() -> Self {
        Self {
            releases: HashMap::new(),
            complete: true,
        }
    }

    pub fn incomplete() -> Self {
        Self {
            complete: false,
            ..Self::new()
        }
    }

    pub fn get(&self, number: &i64) -> Option<&ReleaseRef> {
        self.releases.get(number)
    }

    pub fn insert(&mut self, number: i64, release: ReleaseRef) {
        self.releases.insert(number, release);
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }
}

impl Default for ReleaseMap {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum FetchError {
    Upcoming(anyhow::Error),
    Release { tag: String, error: anyhow::Error },
}

impl FetchError {
    pub(crate) fn error(&self) -> &anyhow::Error {
        match self {
            Self::Upcoming(error) | Self::Release { error, .. } => error,
        }
    }

    pub(crate) fn rate_limited(&self) -> bool {
        is_rate_limited(self.error())
    }
}

fn is_rate_limited(error: &anyhow::Error) -> bool {
    format!("{error:#}")
        .to_ascii_lowercase()
        .contains("rate limit")
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

#[derive(Deserialize, serde::Serialize)]
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

#[derive(Deserialize, serde::Serialize)]
struct Comparison {
    total_commits: usize,
    commits: Vec<CommitNode>,
}

#[derive(Deserialize, serde::Serialize)]
struct CommitNode {
    author: Option<Author>,
    commit: CommitMeta,
}

#[derive(Deserialize, serde::Serialize)]
struct CommitMeta {
    message: String,
}

#[derive(Deserialize, serde::Serialize)]
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
    let (_, number) = subject.trim_end().strip_suffix(')')?.rsplit_once("(#")?;
    number.parse().ok()
}

/// First-line PR number of a commit, if any.
fn commit_pr(node: &CommitNode) -> Option<u32> {
    node.commit.message.lines().next().and_then(pr_number)
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

/// Build the release map for the displayed merged PRs, stopping when all of
/// them are mapped. An unmapped PR can still be unreleased, so it requires the
/// full scan; an incomplete range makes unmapped PRs unknown.
pub(crate) fn fetch_release_map(
    client: &Client,
    repo: &Repo,
    me: &str,
    include_prereleases: bool,
    prs: &[i64],
) -> Result<(ReleaseMap, Vec<FetchError>)> {
    let mut missing: HashSet<_> = prs.iter().copied().collect();
    let mut map = ReleaseMap::new();
    if missing.is_empty() {
        return Ok((map, Vec::new()));
    }

    let tags = release_tags(client, repo, include_prereleases)?;
    let base_url = format!("https://github.com/{}/{}", repo.owner, repo.name);
    let mut errors = Vec::new();
    for (i, tag) in tags.iter().enumerate().take(RELEASES) {
        let base = tags.get(i + 1).map(|t| t.name.as_str());
        let (count, prs) = match range_mine(client, repo, me, base, &tag.name) {
            Ok(result) => result,
            Err(error) => {
                map.complete = false;
                let error = error.context(format!("counting release {}", tag.name));
                let rate_limited = is_rate_limited(&error);
                errors.push(FetchError::Release {
                    tag: tag.name.clone(),
                    error,
                });
                if rate_limited {
                    break;
                }
                continue;
            }
        };
        if count.capped {
            map.complete = false;
        }
        let url = format!("{base_url}/releases/tag/{}", tag.name);
        for n in prs {
            let n = i64::from(n);
            if missing.remove(&n) {
                map.releases.insert(
                    n,
                    ReleaseRef {
                        tag: tag.name.clone(),
                        url: url.clone(),
                    },
                );
            }
        }
        if missing.is_empty() {
            break;
        }
    }
    Ok((map, errors))
}

/// Compute the commit stats for `repo` (my work in each of the last
/// [`RELEASES`] releases — stable only, or including prereleases when
/// `include_prereleases` is set — and, when `default_branch` is provided, in the
/// next unreleased version), plus a map from PR number to the release that shipped
/// it (for annotating the merged section). An absent PR is confirmed unshipped
/// only when the release map is complete.
pub fn fetch(
    client: &Client,
    repo: &Repo,
    me: &str,
    default_branch: Option<&str>,
    include_prereleases: bool,
) -> Result<(CommitStats, ReleaseMap, Vec<FetchError>)> {
    let tags = release_tags(client, repo, include_prereleases)?;
    let base_url = format!("https://github.com/{}/{}", repo.owner, repo.name);
    let mut map = ReleaseMap::new();
    let mut errors = Vec::new();

    // The next release is everything since the latest tag (or the whole default
    // branch when there are no releases yet); its label links to that log. Its
    // PRs are unreleased, so they are not added to the map.
    let upcoming = if let Some(default_branch) = default_branch {
        let latest = tags.first().map(|t| t.name.as_str());
        match range_mine(client, repo, me, latest, default_branch) {
            Ok((count, _)) => Some(Bucket {
                count,
                url: match latest {
                    Some(latest) => format!("{base_url}/compare/{latest}...{default_branch}"),
                    None => format!("{base_url}/commits/{default_branch}"),
                },
            }),
            Err(error) => {
                errors.push(FetchError::Upcoming(error));
                None
            }
        }
    } else {
        None
    };

    // Each shipped release is the range between it and the release before it;
    // the oldest tag we know of has no predecessor, so count everything up to it.
    let mut releases = Vec::with_capacity(tags.len().min(RELEASES));
    let mut stop_after_rate_limit = errors.last().is_some_and(FetchError::rate_limited);
    for (i, tag) in tags.iter().enumerate().take(RELEASES) {
        let url = format!("{base_url}/releases/tag/{}", tag.name);
        if stop_after_rate_limit {
            map.complete = false;
            errors.push(FetchError::Release {
                tag: tag.name.clone(),
                error: anyhow::anyhow!("skipped after GitHub rate limit"),
            });
            releases.push(Release {
                tag: tag.name.clone(),
                count: None,
                url,
                published_at: tag.published_at.clone(),
            });
            continue;
        }
        let base = tags.get(i + 1).map(|t| t.name.as_str());
        let (count, prs) = match range_mine(client, repo, me, base, &tag.name) {
            Ok(result) => result,
            Err(error) => {
                map.complete = false;
                let error = error.context(format!("counting release {}", tag.name));
                stop_after_rate_limit = is_rate_limited(&error);
                errors.push(FetchError::Release {
                    tag: tag.name.clone(),
                    error,
                });
                releases.push(Release {
                    tag: tag.name.clone(),
                    count: None,
                    url,
                    published_at: tag.published_at.clone(),
                });
                continue;
            }
        };
        if count.capped {
            map.complete = false;
        }
        // Newest release first, and ranges are disjoint, so a PR maps to the
        // earliest release that ships it.
        for n in prs {
            map.releases
                .entry(i64::from(n))
                .or_insert_with(|| ReleaseRef {
                    tag: tag.name.clone(),
                    url: url.clone(),
                });
        }
        releases.push(Release {
            tag: tag.name.clone(),
            count: Some(count),
            url,
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
        errors,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::fake;

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
        assert_eq!(pr_number("follow up (#7) (#12)"), Some(12));
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

    #[test]
    fn conditional_shipment_requests_reuse_unchanged_data() {
        const RELEASES: &str = r#"[
            {"tag_name":"v1.4","draft":false,"prerelease":false},
            {"tag_name":"v1.3","draft":false,"prerelease":false},
            {"tag_name":"v1.2","draft":false,"prerelease":false},
            {"tag_name":"v1.1","draft":false,"prerelease":false}
        ]"#;
        const COMPARE: &str = r#"{
            "total_commits":1,
            "commits":[{"author":{"login":"me"},"commit":{"message":"feat (#1)"}}]
        }"#;
        const COMMITS: &str = r#"[{"author":{"login":"me"},"commit":{"message":"feat (#1)"}}]"#;
        const RELEASES_PATH: &str = "GET /repos/owner/repo/releases?per_page=50&page=1";
        const UPCOMING_PATH: &str = "GET /repos/owner/repo/compare/v1.4...main";
        const RELEASE_1_PATH: &str = "GET /repos/owner/repo/compare/v1.3...v1.4";
        const RELEASE_2_PATH: &str = "GET /repos/owner/repo/compare/v1.2...v1.3";
        const RELEASE_3_PATH: &str = "GET /repos/owner/repo/compare/v1.1...v1.2";
        const COMMITS_PATH: &str =
            "GET /repos/owner/repo/commits?sha=v1.1&author=me&per_page=100&page=1";
        let routes = [
            (RELEASES_PATH, RELEASES),
            (UPCOMING_PATH, COMPARE),
            (RELEASE_1_PATH, COMPARE),
            (RELEASE_2_PATH, COMPARE),
            (RELEASE_3_PATH, COMPARE),
            (COMMITS_PATH, COMMITS),
        ];
        let (client, fake) = fake::client(
            routes
                .iter()
                .map(|(path, body)| (path.to_string(), body.to_string())),
        );
        for (path, body) in routes {
            fake.set_response(path, 200, &[("etag", "v1")], body);
        }

        let repo = Repo::parse("owner/repo").unwrap();
        let first = fetch(&client, &repo, "me", Some("main"), false).unwrap();
        let first_requests = fake.requests();
        assert_eq!(first_requests, routes.map(|(path, _)| path.to_string()));
        assert_eq!(
            fake.validators()[..routes.len()],
            routes.map(|(path, _)| (path.to_string(), None))
        );

        for (path, _) in routes {
            fake.set_response(path, 304, &[], "");
        }
        let second = fetch(&client, &repo, "me", Some("main"), false).unwrap();
        assert_eq!(second.0, first.0);
        assert_eq!(second.1, first.1);
        assert!(second.2.is_empty());
        assert_eq!(fake.requests()[6..], first_requests);
        assert_eq!(
            fake.validators()[routes.len()..],
            routes.map(|(path, _)| (path.to_string(), Some("v1".to_string())))
        );

        fake.set_response(
            UPCOMING_PATH,
            200,
            &[("etag", "v2")],
            r#"{"total_commits":2,"commits":[{"author":{"login":"me"},"commit":{"message":"feat (#1)"}}]}"#,
        );
        let third = fetch(&client, &repo, "me", Some("main"), false).unwrap();
        assert_ne!(third.0, second.0);
        assert_eq!(fake.requests()[12..], first_requests);

        for (path, _) in routes {
            fake.set_response(path, 304, &[], "");
        }
        let fourth = fetch(&client, &repo, "me", Some("main"), false).unwrap();
        assert_eq!(fourth.0, third.0);
        assert_eq!(fourth.1, third.1);
        assert!(fourth.2.is_empty());
        assert_eq!(fake.requests()[18..], first_requests);
        assert_eq!(
            fake.validators()[18..],
            [
                (RELEASES_PATH.to_string(), Some("v1".to_string())),
                (UPCOMING_PATH.to_string(), Some("v2".to_string())),
                (RELEASE_1_PATH.to_string(), Some("v1".to_string())),
                (RELEASE_2_PATH.to_string(), Some("v1".to_string())),
                (RELEASE_3_PATH.to_string(), Some("v1".to_string())),
                (COMMITS_PATH.to_string(), Some("v1".to_string())),
            ]
        );
    }

    fn four_release_routes() -> Vec<(String, String)> {
        let tags = ["v4", "v3", "v2", "v1"];
        let releases = tags
            .iter()
            .map(|tag| serde_json::json!({"tag_name": tag, "draft": false, "prerelease": false}))
            .collect::<Vec<_>>();
        let mut routes = vec![(
            "GET /repos/owner/repo/releases?per_page=50&page=1".into(),
            serde_json::Value::Array(releases).to_string(),
        )];
        routes.push((
            "GET /repos/owner/repo/compare/v4...main".into(),
            r#"{"total_commits":0,"commits":[]}"#.into(),
        ));
        for (base, head, number) in [("v3", "v4", 40), ("v2", "v3", 30), ("v1", "v2", 20)] {
            routes.push((
                format!("GET /repos/owner/repo/compare/{base}...{head}"),
                serde_json::json!({
                    "total_commits": 1,
                    "commits": [{
                        "author": {"login": "me"},
                        "commit": {"message": format!("feat (#{number})")}
                    }]
                })
                .to_string(),
            ));
        }
        routes.push((
            "GET /repos/owner/repo/commits?sha=v1&author=me&per_page=100&page=1".into(),
            serde_json::json!([{
                "author": {"login": "me"},
                "commit": {"message": "feat (#10)"}
            }])
            .to_string(),
        ));
        routes
    }

    #[test]
    fn release_range_failures_preserve_other_buckets() {
        let paths = [
            "GET /repos/owner/repo/compare/v3...v4",
            "GET /repos/owner/repo/compare/v2...v3",
            "GET /repos/owner/repo/compare/v1...v2",
            "GET /repos/owner/repo/commits?sha=v1&author=me&per_page=100&page=1",
        ];
        let repo = Repo::parse("owner/repo").unwrap();

        for (failed_index, path) in paths.into_iter().enumerate() {
            let (client, fake) = fake::client(four_release_routes());
            fake.set_response(path, 500, &[], r#"{"message":"injected range failure"}"#);

            let (stats, map, errors) = fetch(&client, &repo, "me", Some("main"), false).unwrap();

            assert!(stats.available);
            assert_eq!(stats.upcoming.as_ref().unwrap().count.mine, 0);
            assert_eq!(stats.releases.len(), 4);
            for (index, release) in stats.releases.iter().enumerate() {
                assert_eq!(
                    release.count.is_some(),
                    index != failed_index,
                    "{}",
                    release.tag
                );
            }
            assert!(!map.is_complete());
            assert_eq!(errors.len(), 1);
            assert_eq!(fake.requests().len(), 6);
        }
    }

    #[test]
    fn capped_shipped_range_keeps_lower_bound_and_marks_map_incomplete() {
        let repo = Repo::parse("owner/repo").unwrap();
        let (client, fake) = fake::client(four_release_routes());
        fake.set_response(
            "GET /repos/owner/repo/compare/v3...v4",
            200,
            &[],
            r#"{"total_commits":300,"commits":[{"author":{"login":"me"},"commit":{"message":"feat (#40)"}}]}"#,
        );

        let (stats, map, errors) = fetch(&client, &repo, "me", Some("main"), false).unwrap();

        assert!(stats.releases[0].count.unwrap().capped);
        assert!(!map.is_complete());
        assert!(errors.is_empty());
        assert_eq!(fake.requests().len(), 6);
    }

    #[test]
    fn capped_hidden_release_range_marks_unmapped_prs_unknown() {
        let repo = Repo::parse("owner/repo").unwrap();
        let (client, fake) = fake::client(four_release_routes());
        fake.set_response(
            "GET /repos/owner/repo/compare/v3...v4",
            200,
            &[],
            r#"{"total_commits":300,"commits":[{"author":{"login":"me"},"commit":{"message":"feat (#40)"}}]}"#,
        );

        let (map, errors) = fetch_release_map(&client, &repo, "me", false, &[99]).unwrap();

        assert!(!map.is_complete());
        assert!(map.get(&99).is_none());
        assert!(errors.is_empty());
        assert_eq!(fake.requests().len(), 5);
    }

    #[test]
    fn failed_upcoming_range_does_not_discard_shipped_buckets() {
        let repo = Repo::parse("owner/repo").unwrap();
        let (client, fake) = fake::client(four_release_routes());
        fake.set_response(
            "GET /repos/owner/repo/compare/v4...main",
            500,
            &[],
            r#"{"message":"injected upcoming failure"}"#,
        );

        let (stats, map, errors) = fetch(&client, &repo, "me", Some("main"), false).unwrap();

        assert!(stats.available);
        assert!(stats.upcoming.is_none());
        assert!(stats.releases.iter().all(|release| release.count.is_some()));
        assert!(map.is_complete());
        assert!(matches!(errors.as_slice(), [FetchError::Upcoming(_)]));
        assert_eq!(fake.requests().len(), 6);
    }

    #[test]
    fn rate_limit_stops_remaining_release_ranges() {
        let repo = Repo::parse("owner/repo").unwrap();
        let (client, fake) = fake::client(four_release_routes());
        fake.set_response(
            "GET /repos/owner/repo/compare/v3...v4",
            403,
            &[
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", "4102444800"),
            ],
            r#"{"message":"API rate limit exceeded"}"#,
        );

        let (stats, map, errors) = fetch(&client, &repo, "me", Some("main"), false).unwrap();

        assert!(stats.releases.iter().all(|release| release.count.is_none()));
        assert!(!map.is_complete());
        assert_eq!(fake.requests().len(), 3);
        assert_eq!(errors.len(), 4);
        assert!(errors.iter().all(FetchError::rate_limited));
    }

    #[test]
    fn rate_limit_stops_hidden_release_lookup() {
        let repo = Repo::parse("owner/repo").unwrap();
        let (client, fake) = fake::client(four_release_routes());
        fake.set_response(
            "GET /repos/owner/repo/compare/v3...v4",
            403,
            &[
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", "4102444800"),
            ],
            r#"{"message":"API rate limit exceeded"}"#,
        );

        let (map, errors) =
            fetch_release_map(&client, &repo, "me", false, &[40, 30, 20, 10]).unwrap();

        assert!(!map.is_complete());
        assert_eq!(fake.requests().len(), 2);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].rate_limited());
    }

    #[test]
    fn release_list_pagination_failure_does_not_scan_partial_tags() {
        let repo = Repo::parse("owner/repo").unwrap();
        let drafts = serde_json::Value::Array(
            (0..50)
                .map(|n| {
                    serde_json::json!({
                        "tag_name": format!("draft-{n}"),
                        "draft": true,
                        "prerelease": false
                    })
                })
                .collect(),
        );
        let (client, fake) = fake::client([(
            "GET /repos/owner/repo/releases?per_page=50&page=1".into(),
            drafts.to_string(),
        )]);
        fake.set_response(
            "GET /repos/owner/repo/releases?per_page=50&page=2",
            500,
            &[],
            r#"{"message":"injected release-list failure"}"#,
        );

        assert!(fetch(&client, &repo, "me", Some("main"), false).is_err());
        assert_eq!(
            fake.requests(),
            [
                "GET /repos/owner/repo/releases?per_page=50&page=1",
                "GET /repos/owner/repo/releases?per_page=50&page=2",
            ]
        );
    }

    #[test]
    fn later_reachable_commit_page_failure_marks_only_upcoming_unavailable() {
        let repo = Repo::parse("owner/repo").unwrap();
        let commits = serde_json::Value::Array(
            (0..100)
                .map(|n| {
                    serde_json::json!({
                        "author": {"login": "me"},
                        "commit": {"message": format!("commit {n}")}
                    })
                })
                .collect(),
        );
        let (client, fake) = fake::client([
            (
                "GET /repos/owner/repo/releases?per_page=50&page=1".into(),
                "[]".into(),
            ),
            (
                "GET /repos/owner/repo/commits?sha=main&author=me&per_page=100&page=1".into(),
                commits.to_string(),
            ),
        ]);
        fake.set_response(
            "GET /repos/owner/repo/commits?sha=main&author=me&per_page=100&page=2",
            500,
            &[],
            r#"{"message":"injected page failure"}"#,
        );

        let (stats, map, errors) = fetch(&client, &repo, "me", Some("main"), false).unwrap();

        assert!(stats.available);
        assert!(stats.upcoming.is_none());
        assert_eq!(stats.releases, Vec::<Release>::new());
        assert!(map.is_complete());
        assert!(matches!(errors.as_slice(), [FetchError::Upcoming(_)]));
        assert_eq!(fake.requests().len(), 3);
    }
}

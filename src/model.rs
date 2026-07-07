//! Typed serde models for the three GitHub GraphQL queries, plus the fetch
//! helpers that run them. Queries are sent verbatim (the merged query's page
//! size is the only thing we interpolate, so `--merged-limit` is honored).

use crate::github::{Client, Repo};
use anyhow::Result;
use serde::Deserialize;

// ----------------------------------------------------------------------------
// Merge queue
// ----------------------------------------------------------------------------

pub const QUEUE_QUERY: &str = r#"query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    mergeQueue {
      entries(first: 100) {
        nodes {
          position
          enqueuedAt
          headCommit { committedDate }
          pullRequest { number title url author { login } }
        }
      }
    }
  }
}"#;

#[derive(Debug, Deserialize)]
pub struct QueueData {
    pub repository: Option<QueueRepo>,
}

#[derive(Debug, Deserialize)]
pub struct QueueRepo {
    #[serde(rename = "mergeQueue")]
    pub merge_queue: Option<MergeQueue>,
}

#[derive(Debug, Deserialize)]
pub struct MergeQueue {
    pub entries: QueueEntries,
}

#[derive(Debug, Deserialize)]
pub struct QueueEntries {
    pub nodes: Vec<QueueEntryNode>,
}

#[derive(Debug, Deserialize)]
pub struct QueueEntryNode {
    pub position: i64,
    /// When the entry joined the queue; drives the WAIT column.
    #[serde(rename = "enqueuedAt")]
    pub enqueued_at: Option<String>,
    /// The speculative merge commit the queue is building; its `committedDate`
    /// approximates when the build for this entry started (the BUILD column).
    #[serde(rename = "headCommit")]
    pub head_commit: Option<QueueCommit>,
    #[serde(rename = "pullRequest")]
    pub pull_request: QueuePr,
}

#[derive(Debug, Deserialize)]
pub struct QueueCommit {
    #[serde(rename = "committedDate")]
    pub committed_date: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct QueuePr {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub author: Option<Login>,
}

#[derive(Debug, Deserialize)]
pub struct Login {
    pub login: String,
}

/// Extract the entry nodes from a parsed queue response. A null queue or an
/// empty queue both yield `[]`.
pub fn queue_nodes(data: QueueData) -> Vec<QueueEntryNode> {
    data.repository
        .and_then(|r| r.merge_queue)
        .map(|q| q.entries.nodes)
        .unwrap_or_default()
}

/// Fetch merge-queue entries. A null queue or empty queue both yield `[]`.
pub fn fetch_queue(client: &Client, repo: &Repo) -> Result<Vec<QueueEntryNode>> {
    let data: QueueData = client.graphql(
        QUEUE_QUERY,
        serde_json::json!({ "owner": repo.owner, "name": repo.name }),
    )?;
    Ok(queue_nodes(data))
}

// ----------------------------------------------------------------------------
// My open PRs
// ----------------------------------------------------------------------------

pub const MINE_QUERY: &str = r#"query($q: String!) {
  search(type: ISSUE, first: 50, query: $q) {
    nodes {
      ... on PullRequest {
        number title url state mergeable mergeStateStatus isDraft updatedAt
        mergeQueueEntry { position state }
        commits(last: 1) { nodes { commit { checkSuites(first: 50) { totalCount nodes { conclusion checkRuns(first: 1) { totalCount } } } } } }
      }
    }
  }
}"#;

#[derive(Debug, Deserialize)]
pub struct MineData {
    pub search: MineNodes,
}

#[derive(Debug, Deserialize)]
pub struct MineNodes {
    pub nodes: Vec<PrNode>,
}

#[derive(Debug, Deserialize)]
pub struct PrNode {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub state: Option<String>,
    pub mergeable: Option<String>,
    #[serde(rename = "mergeStateStatus")]
    pub merge_state_status: Option<String>,
    #[serde(rename = "isDraft")]
    pub is_draft: bool,
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<String>,
    #[serde(rename = "mergeQueueEntry")]
    pub merge_queue_entry: Option<QueueEntry>,
    pub commits: Commits,
}

#[derive(Debug, Deserialize)]
pub struct QueueEntry {
    pub position: i64,
    pub state: String,
}

#[derive(Debug, Deserialize)]
pub struct Commits {
    pub nodes: Vec<CommitNode>,
}

#[derive(Debug, Deserialize)]
pub struct CommitNode {
    pub commit: Commit,
}

#[derive(Debug, Deserialize)]
pub struct Commit {
    #[serde(rename = "checkSuites")]
    pub check_suites: CheckSuites,
}

#[derive(Debug, Deserialize)]
pub struct CheckSuites {
    /// Total suites the server reports, which can exceed the page we fetched.
    /// Used to detect a truncated page so a dropped suite can't render green.
    #[serde(rename = "totalCount")]
    pub total_count: u64,
    pub nodes: Vec<CheckSuite>,
}

#[derive(Debug, Deserialize)]
pub struct CheckSuite {
    pub conclusion: Option<String>,
    /// `null` when the viewer can't see a suite's runs (e.g. a third-party
    /// app's checks); treated the same as a zero-run phantom suite.
    #[serde(rename = "checkRuns")]
    pub check_runs: Option<CheckRuns>,
}

/// How many check runs a suite produced. Suites with zero runs are phantom
/// subscriptions (apps that registered but never ran, or workflows that failed
/// to start) — GitHub's own status rollup ignores them, and so do we.
#[derive(Debug, Deserialize)]
pub struct CheckRuns {
    #[serde(rename = "totalCount")]
    pub total_count: u64,
}

pub fn mine_search(repo: &Repo, me: &str) -> String {
    format!(
        "repo:{}/{} is:pr is:open author:{} archived:false sort:updated-desc",
        repo.owner, repo.name, me
    )
}

pub fn fetch_my_prs(client: &Client, repo: &Repo, me: &str) -> Result<Vec<PrNode>> {
    let q = mine_search(repo, me);
    let data: MineData = client.graphql(MINE_QUERY, serde_json::json!({ "q": q }))?;
    Ok(data.search.nodes)
}

// ----------------------------------------------------------------------------
// Recently merged
// ----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct MergedData {
    pub search: MergedNodes,
}

#[derive(Debug, Deserialize)]
pub struct MergedNodes {
    pub nodes: Vec<MergedNode>,
}

#[derive(Debug, Deserialize)]
pub struct MergedNode {
    pub number: i64,
    pub title: String,
    pub url: String,
    /// PR author; used by the reviewed-and-merged section (the Mine merged
    /// section ignores it, since those are all the viewer's own PRs).
    pub author: Option<Login>,
    #[serde(rename = "mergedAt")]
    pub merged_at: Option<String>,
}

/// The recently-merged query; `first` is the page size (clamped 1..=100). Used
/// for both the "my merged PRs" and "reviewed & merged" sections (only the
/// search query differs).
pub fn merged_query(limit: usize) -> String {
    let first = limit.clamp(1, 100);
    format!(
        r#"query($q: String!) {{
  search(type: ISSUE, first: {first}, query: $q) {{
    nodes {{
      ... on PullRequest {{
        number title url mergedAt author {{ login }}
      }}
    }}
  }}
}}"#
    )
}

pub fn merged_search(repo: &Repo, me: &str, since: &str) -> String {
    // GitHub search can't sort by merge time, but a merge bumps `updatedAt` and
    // later edits only bump it further, so `updated-desc` still surfaces the most
    // recently merged PRs when the result is capped (rows are re-sorted by
    // `mergedAt` for display).
    format!(
        "repo:{}/{} is:pr is:merged author:{} merged:>={} sort:updated-desc",
        repo.owner, repo.name, me, since
    )
}

pub fn fetch_merged(
    client: &Client,
    repo: &Repo,
    me: &str,
    since: &str,
    limit: usize,
) -> Result<Vec<MergedNode>> {
    let q = merged_search(repo, me, since);
    let data: MergedData = client.graphql(&merged_query(limit), serde_json::json!({ "q": q }))?;
    Ok(data.search.nodes)
}

/// The "merged PRs I reviewed" search: merged PRs in the repo that I reviewed,
/// excluding my own (those live in the Mine view). Same shape as `merged_search`.
pub fn reviewed_merged_search(repo: &Repo, me: &str, since: &str) -> String {
    format!(
        "repo:{}/{} is:pr is:merged reviewed-by:{} -author:{} merged:>={} sort:updated-desc",
        repo.owner, repo.name, me, me, since
    )
}

pub fn fetch_reviewed_merged(
    client: &Client,
    repo: &Repo,
    me: &str,
    since: &str,
    limit: usize,
) -> Result<Vec<MergedNode>> {
    let q = reviewed_merged_search(repo, me, since);
    let data: MergedData = client.graphql(&merged_query(limit), serde_json::json!({ "q": q }))?;
    Ok(data.search.nodes)
}

// ----------------------------------------------------------------------------
// Reviews (open PRs awaiting / under my review)
// ----------------------------------------------------------------------------

/// Two aliased searches in one request: PRs whose review is requested from me
/// (`requested`) and PRs I've already reviewed (`reviewed`). A PR can appear in
/// both (a re-review). Each node carries my own reviews (so we know if/when I
/// reviewed) and its last commit date (so we can flag "updated since").
pub const REVIEWS_QUERY: &str = r#"query($me: String!, $requested: String!, $reviewed: String!) {
  requested: search(type: ISSUE, first: 50, query: $requested) { nodes { ...rev } }
  reviewed: search(type: ISSUE, first: 50, query: $reviewed) { nodes { ...rev } }
}
fragment rev on PullRequest {
  number title url isDraft updatedAt
  author { login }
  commits(last: 1) { nodes { commit { committedDate } } }
  reviews(author: $me, first: 20, states: [APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]) { nodes { submittedAt } }
}"#;

#[derive(Debug, Deserialize)]
pub struct ReviewsData {
    /// PRs requesting my review (directly or via a team, per the scope).
    pub requested: ReviewSearch,
    /// PRs I have already reviewed.
    pub reviewed: ReviewSearch,
}

#[derive(Debug, Deserialize)]
pub struct ReviewSearch {
    pub nodes: Vec<ReviewPrNode>,
}

#[derive(Debug, Deserialize)]
pub struct ReviewPrNode {
    pub number: i64,
    pub title: String,
    pub url: String,
    #[serde(rename = "isDraft")]
    pub is_draft: bool,
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<String>,
    pub author: Option<Login>,
    pub commits: ReviewCommits,
    /// My reviews on this PR (submitted only; the query filters out PENDING).
    pub reviews: MyReviews,
}

#[derive(Debug, Deserialize)]
pub struct ReviewCommits {
    pub nodes: Vec<ReviewCommitNode>,
}

#[derive(Debug, Deserialize)]
pub struct ReviewCommitNode {
    pub commit: ReviewCommit,
}

#[derive(Debug, Deserialize)]
pub struct ReviewCommit {
    #[serde(rename = "committedDate")]
    pub committed_date: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MyReviews {
    pub nodes: Vec<MyReview>,
}

#[derive(Debug, Deserialize)]
pub struct MyReview {
    #[serde(rename = "submittedAt")]
    pub submitted_at: Option<String>,
}

/// The two review searches: PRs requesting my review and PRs I have reviewed,
/// both open, in this repo, excluding my own. `requested_qualifier` is
/// `review-requested` (me + my teams) or `user-review-requested` (only me).
pub fn reviews_searches(repo: &Repo, me: &str, requested_qualifier: &str) -> (String, String) {
    let requested = format!(
        "repo:{}/{} is:pr is:open {}:{} -author:{} archived:false sort:updated-desc",
        repo.owner, repo.name, requested_qualifier, me, me
    );
    let reviewed = format!(
        "repo:{}/{} is:pr is:open reviewed-by:{} -author:{} archived:false sort:updated-desc",
        repo.owner, repo.name, me, me
    );
    (requested, reviewed)
}

pub fn fetch_reviews(
    client: &Client,
    repo: &Repo,
    me: &str,
    requested_qualifier: &str,
) -> Result<ReviewsData> {
    let (requested, reviewed) = reviews_searches(repo, me, requested_qualifier);
    client.graphql(
        REVIEWS_QUERY,
        serde_json::json!({ "me": me, "requested": requested, "reviewed": reviewed }),
    )
}

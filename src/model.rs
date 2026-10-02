//! Typed serde models for the GitHub GraphQL queries, plus the fetch
//! helpers that run them. Most queries are sent verbatim; the merged query
//! interpolates its page size, and required-check queries batch one aliased
//! commit lookup per pull request.

use crate::cli::OpenSort;
use crate::github::{Client, Repo};
use crate::status::{self, Approval, Checks};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt::Write as _;

// ----------------------------------------------------------------------------
// Merge queue
// ----------------------------------------------------------------------------

pub const QUEUE_QUERY: &str = r#"query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    mergeQueue {
      nextEntryEstimatedTimeToMerge
      entries(first: 100) {
        nodes {
          position
          enqueuedAt
          headCommit {
            id
            statusCheckRollup {
              contexts(first: 40) {
                checkRunCountsByState { state count }
                statusContextCountsByState { state count }
                nodes { ... on CheckRun { startedAt } }
              }
            }
          }
          pullRequest { number title url headRefName author { login } }
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
    /// Seconds until a newly added entry would merge; shown by the queue header.
    #[serde(rename = "nextEntryEstimatedTimeToMerge")]
    pub next_entry_estimated_time_to_merge: Option<i64>,
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
    /// The speculative merge commit the queue is building; its checks' `startedAt`
    /// mark when CI actually began (the BUILD column). `None` until the entry has
    /// a speculative commit.
    #[serde(rename = "headCommit")]
    pub head_commit: Option<QueueCommit>,
    #[serde(rename = "pullRequest")]
    pub pull_request: QueuePr,
    #[serde(skip)]
    pub(crate) required: Option<RequiredSummary>,
}

#[derive(Debug, Deserialize)]
pub struct QueueCommit {
    pub id: String,
    /// Flat rollup of every check on the commit. Preferred over `checkSuites`:
    /// it is a single (cheaper) connection and front-loads the actual check
    /// runs, whereas the first check suites are often app integrations with no
    /// started run. `None` when the commit has no checks configured.
    #[serde(rename = "statusCheckRollup")]
    pub status_check_rollup: Option<QueueRollup>,
}

#[derive(Debug, Deserialize)]
pub struct QueueRollup {
    pub contexts: QueueContexts,
}

#[derive(Debug, Deserialize)]
pub struct QueueContexts {
    /// GitHub's own per-state check-run counts: exact and unpaginated, unlike
    /// `nodes` (which is capped and only read for the earliest `startedAt`).
    #[serde(rename = "checkRunCountsByState", default)]
    pub check_runs: Vec<StateCount>,
    #[serde(rename = "statusContextCountsByState", default)]
    pub status_contexts: Vec<StateCount>,
    pub nodes: Vec<QueueContext>,
}

/// One rollup context. Only a `CheckRun` carries `startedAt`; a legacy
/// `StatusContext` has no such field and deserializes to `None`.
#[derive(Debug, Deserialize)]
pub struct QueueContext {
    #[serde(rename = "startedAt")]
    pub started_at: Option<String>,
}

impl QueueEntryNode {
    /// Failing / running / passing checks on the speculative merge commit —
    /// the queue's own CI semaphore. Empty when the entry has no speculative
    /// commit or no checks yet.
    pub fn checks(&self) -> Checks {
        if let Some(summary) = &self.required {
            return summary.checks;
        }
        let Some(rollup) = self
            .head_commit
            .as_ref()
            .and_then(|h| h.status_check_rollup.as_ref())
        else {
            return Checks::default();
        };
        checks_from_counts(
            &rollup.contexts.check_runs,
            &rollup.contexts.status_contexts,
        )
    }

    /// The earliest moment any check on the speculative merge commit began
    /// running (RFC 3339). `None` when nothing has started yet (or there is no
    /// speculative commit / no checks), which the BUILD column renders as a
    /// dash. RFC 3339 `...Z` timestamps sort lexically == chronologically, so
    /// `min` is earliest.
    pub fn build_started_at(&self) -> Option<String> {
        if let Some(summary) = &self.required {
            return summary.build_started_at.clone();
        }
        self.head_commit
            .as_ref()?
            .status_check_rollup
            .as_ref()?
            .contexts
            .nodes
            .iter()
            .filter_map(|c| c.started_at.as_deref())
            .min()
            .map(str::to_owned)
    }
}

#[derive(Debug, Deserialize)]
pub struct QueuePr {
    pub number: i64,
    pub title: String,
    pub url: String,
    #[serde(rename = "headRefName")]
    pub head_ref_name: Option<String>,
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

/// The queue-level estimate (seconds until a newly added entry would merge),
/// or `None` when there is no queue or the API omits it.
pub fn queue_next_eta(data: &QueueData) -> Option<i64> {
    data.repository
        .as_ref()?
        .merge_queue
        .as_ref()?
        .next_entry_estimated_time_to_merge
}

/// Fetch the merge-queue entries and the queue-level ETA. A null or empty queue
/// yields `([], None)`.
pub fn fetch_queue(
    client: &Client,
    repo: &Repo,
    required_only: bool,
) -> Result<(Vec<QueueEntryNode>, Option<i64>)> {
    let data: QueueData = client.graphql(
        QUEUE_QUERY,
        serde_json::json!({ "owner": repo.owner, "name": repo.name }),
    )?;
    let eta = queue_next_eta(&data);
    let mut nodes = queue_nodes(data);
    if required_only {
        let (indices, targets): (Vec<_>, Vec<_>) = nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| {
                node.head_commit.as_ref().map(|commit| {
                    (
                        index,
                        RequiredTarget {
                            pull_request_number: node.pull_request.number,
                            commit_id: commit.id.clone(),
                        },
                    )
                })
            })
            .unzip();
        for (index, summary) in indices.into_iter().zip(fetch_required(client, targets)?) {
            nodes[index].required = Some(summary);
        }
    }
    Ok((nodes, eta))
}

// ----------------------------------------------------------------------------
// My open PRs
// ----------------------------------------------------------------------------

pub const MINE_QUERY: &str = r#"query($q: String!) {
  search(type: ISSUE, first: 50, query: $q) {
    nodes {
      ... on PullRequest {
        number title url mergeable mergeStateStatus isDraft updatedAt createdAt headRefName
        reviewDecision
        latestOpinionatedReviews(first: 100) { nodes { state } }
        mergeQueueEntry { position state }
        reviewThreads(first: 100) { totalCount nodes { isResolved } }
        commits(last: 1) { nodes { commit { id statusCheckRollup { contexts(first: 1) {
          checkRunCountsByState { state count }
          statusContextCountsByState { state count }
        } } } } }
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
    pub mergeable: Option<String>,
    #[serde(rename = "mergeStateStatus")]
    pub merge_state_status: Option<String>,
    #[serde(rename = "isDraft")]
    pub is_draft: bool,
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: Option<String>,
    /// The PR's head branch.
    #[serde(rename = "headRefName")]
    pub head_ref_name: Option<String>,
    /// Whether the PR satisfies GitHub's required review rules; null without them.
    #[serde(rename = "reviewDecision")]
    pub review_decision: Option<String>,
    /// The latest `APPROVED` / `CHANGES_REQUESTED` review of each reviewer.
    #[serde(rename = "latestOpinionatedReviews", default)]
    pub latest_opinionated_reviews: OpinionatedReviews,
    #[serde(rename = "mergeQueueEntry")]
    pub merge_queue_entry: Option<QueueEntry>,
    #[serde(rename = "reviewThreads")]
    pub review_threads: ReviewThreads,
    pub commits: Commits,
    #[serde(skip)]
    pub(crate) required_checks: Option<Checks>,
}

#[derive(Debug, Deserialize)]
pub struct QueueEntry {
    pub position: i64,
    pub state: String,
}

/// The latest opinionated (approving or change-requesting) review of every
/// reviewer of a PR.
#[derive(Debug, Default, Deserialize)]
pub struct OpinionatedReviews {
    pub nodes: Vec<OpinionatedReview>,
}

#[derive(Debug, Deserialize)]
pub struct OpinionatedReview {
    pub state: String,
}

/// The PR's review threads. There is no "unresolved" aggregate, so we page the
/// first 100 and count them; `total_count` tells us whether that page was
/// complete (a PR with more than 100 threads renders its count as `100+`).
#[derive(Debug, Deserialize)]
pub struct ReviewThreads {
    #[serde(rename = "totalCount")]
    pub total_count: u64,
    pub nodes: Vec<ReviewThread>,
}

#[derive(Debug, Deserialize)]
pub struct ReviewThread {
    #[serde(rename = "isResolved")]
    pub is_resolved: bool,
}

impl ReviewThreads {
    /// (unresolved threads on the fetched page, whether the page was truncated).
    pub fn unresolved(&self) -> (usize, bool) {
        let n = self.nodes.iter().filter(|t| !t.is_resolved).count();
        (n, self.total_count > self.nodes.len() as u64)
    }
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
    pub id: String,
    /// `null` when the commit has no checks or statuses at all.
    #[serde(rename = "statusCheckRollup")]
    pub status_check_rollup: Option<Rollup>,
}

#[derive(Debug, Deserialize)]
pub struct Rollup {
    pub contexts: RollupCounts,
}

/// GitHub's own per-state tallies for the rollup. Using the aggregates instead
/// of paging `contexts` keeps the query cheap and the counts exact — no phantom
/// zero-run check suites, no truncated page to compensate for.
#[derive(Debug, Deserialize)]
pub struct RollupCounts {
    #[serde(rename = "checkRunCountsByState")]
    pub check_runs: Vec<StateCount>,
    #[serde(rename = "statusContextCountsByState")]
    pub status_contexts: Vec<StateCount>,
}

#[derive(Debug, Deserialize)]
pub struct StateCount {
    pub state: String,
    pub count: u64,
}

fn checks_from_counts(check_runs: &[StateCount], status_contexts: &[StateCount]) -> Checks {
    let mut checks = Checks::default();
    for count in check_runs {
        checks.add(status::check_run_lamp(&count.state), count.count);
    }
    for count in status_contexts {
        checks.add(status::status_context_lamp(&count.state), count.count);
    }
    checks
}

impl PrNode {
    /// The failing / running / passing check counts for the PR's last commit.
    /// Check runs and legacy commit statuses are folded into the same semaphore.
    pub fn checks(&self) -> Checks {
        if let Some(checks) = self.required_checks {
            return checks;
        }
        let Some(rollup) = self
            .commits
            .nodes
            .first()
            .and_then(|n| n.commit.status_check_rollup.as_ref())
        else {
            return Checks::default();
        };
        checks_from_counts(
            &rollup.contexts.check_runs,
            &rollup.contexts.status_contexts,
        )
    }

    /// Whether the PR has any approval or all required approvals.
    pub fn approval(&self) -> Approval {
        status::approval_of(
            self.review_decision.as_deref(),
            self.latest_opinionated_reviews
                .nodes
                .iter()
                .map(|r| r.state.as_str()),
        )
    }
}

pub fn mine_search(repo: &Repo, me: &str, sort: OpenSort) -> String {
    format!(
        "repo:{}/{} is:pr is:open author:{} archived:false {}",
        repo.owner,
        repo.name,
        me,
        sort.qualifier()
    )
}

pub fn fetch_my_prs(
    client: &Client,
    repo: &Repo,
    me: &str,
    sort: OpenSort,
    required_only: bool,
) -> Result<Vec<PrNode>> {
    let q = mine_search(repo, me, sort);
    let data: MineData = client.graphql(MINE_QUERY, serde_json::json!({ "q": q }))?;
    let mut nodes = data.search.nodes;
    if required_only {
        let (indices, targets): (Vec<_>, Vec<_>) = nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| {
                node.commits.nodes.first().map(|commit| {
                    (
                        index,
                        RequiredTarget {
                            pull_request_number: node.number,
                            commit_id: commit.commit.id.clone(),
                        },
                    )
                })
            })
            .unzip();
        for (index, summary) in indices.into_iter().zip(fetch_required(client, targets)?) {
            nodes[index].required_checks = Some(summary.checks);
        }
    }
    Ok(nodes)
}

// ----------------------------------------------------------------------------
// Required status checks
// ----------------------------------------------------------------------------

#[derive(Debug)]
struct RequiredTarget {
    pull_request_number: i64,
    commit_id: String,
}

#[derive(Debug, Default)]
pub(crate) struct RequiredSummary {
    pub(crate) checks: Checks,
    pub(crate) build_started_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RequiredCommit {
    #[serde(rename = "statusCheckRollup")]
    status_check_rollup: Option<RequiredRollup>,
}

#[derive(Debug, Deserialize)]
struct RequiredRollup {
    contexts: RequiredContexts,
}

#[derive(Debug, Deserialize)]
struct RequiredContexts {
    #[serde(rename = "pageInfo")]
    page_info: RequiredPageInfo,
    nodes: Vec<RequiredContext>,
}

#[derive(Debug, Deserialize)]
struct RequiredPageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum RequiredContext {
    CheckRun {
        required: bool,
        status: String,
        conclusion: Option<String>,
        #[serde(rename = "startedAt")]
        started_at: Option<String>,
    },
    StatusContext {
        required: bool,
        state: String,
    },
}

impl RequiredContext {
    fn add_to(self, summary: &mut RequiredSummary) {
        match self {
            RequiredContext::CheckRun {
                required: true,
                status,
                conclusion,
                started_at,
            } => {
                summary.checks.add(
                    status::check_run_lamp(conclusion.as_deref().unwrap_or(&status)),
                    1,
                );
                if let Some(started_at) = started_at
                    && summary
                        .build_started_at
                        .as_ref()
                        .is_none_or(|earliest| started_at < *earliest)
                {
                    summary.build_started_at = Some(started_at);
                }
            }
            RequiredContext::StatusContext {
                required: true,
                state,
            } => summary.checks.add(status::status_context_lamp(&state), 1),
            _ => {}
        }
    }
}

#[derive(Debug)]
struct PendingRequired {
    target: usize,
    pull_request_number: i64,
    commit_id: String,
    after: Option<String>,
}

fn required_query(pending: &[PendingRequired]) -> (String, serde_json::Value) {
    let mut declarations = Vec::with_capacity(pending.len() * 3);
    let mut fields = String::new();
    let mut variables = serde_json::Map::new();
    for (alias, item) in pending.iter().enumerate() {
        declarations.extend([
            format!("$commit{alias}: ID!"),
            format!("$pull{alias}: Int!"),
            format!("$after{alias}: String"),
        ]);
        write!(
            fields,
            r#"c{alias}: node(id: $commit{alias}) {{
  ... on Commit {{
    statusCheckRollup {{
      contexts(first: 100, after: $after{alias}) {{
        pageInfo {{ hasNextPage endCursor }}
        nodes {{
          __typename
          ... on CheckRun {{
            required: isRequired(pullRequestNumber: $pull{alias})
            status conclusion startedAt
          }}
          ... on StatusContext {{
            required: isRequired(pullRequestNumber: $pull{alias})
            state
          }}
        }}
      }}
    }}
  }}
}}
"#
        )
        .expect("writing to a String cannot fail");
        variables.insert(
            format!("commit{alias}"),
            serde_json::Value::String(item.commit_id.clone()),
        );
        variables.insert(
            format!("pull{alias}"),
            serde_json::Value::from(item.pull_request_number),
        );
        variables.insert(format!("after{alias}"), serde_json::json!(item.after));
    }
    (
        format!("query({}) {{\n{fields}}}", declarations.join(", ")),
        serde_json::Value::Object(variables),
    )
}

fn fetch_required(client: &Client, targets: Vec<RequiredTarget>) -> Result<Vec<RequiredSummary>> {
    let mut summaries: Vec<_> = (0..targets.len())
        .map(|_| RequiredSummary::default())
        .collect();
    let mut pending: Vec<_> = targets
        .into_iter()
        .enumerate()
        .map(|(target, item)| PendingRequired {
            target,
            pull_request_number: item.pull_request_number,
            commit_id: item.commit_id,
            after: None,
        })
        .collect();

    while !pending.is_empty() {
        let (query, variables) = required_query(&pending);
        let mut data: HashMap<String, Option<RequiredCommit>> =
            client.graphql(&query, variables)?;
        let mut next = Vec::new();
        for (alias, mut item) in pending.into_iter().enumerate() {
            let commit = data
                .remove(&format!("c{alias}"))
                .with_context(|| format!("required-check response omitted c{alias}"))?
                .with_context(|| format!("required-check commit c{alias} was unavailable"))?;
            let Some(rollup) = commit.status_check_rollup else {
                continue;
            };
            for context in rollup.contexts.nodes {
                context.add_to(&mut summaries[item.target]);
            }
            if rollup.contexts.page_info.has_next_page {
                item.after = Some(
                    rollup
                        .contexts
                        .page_info
                        .end_cursor
                        .context("required-check page had no end cursor")?,
                );
                next.push(item);
            }
        }
        pending = next;
    }

    Ok(summaries)
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
    #[serde(rename = "headRefName")]
    pub head_ref_name: Option<String>,
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
        number title url headRefName mergedAt author {{ login }}
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
  number title url headRefName isDraft updatedAt
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
    #[serde(rename = "headRefName")]
    pub head_ref_name: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mine_search_sorts_before_the_api_page_limit() {
        let repo = Repo::parse("owner/repo").unwrap();
        for (sort, qualifier) in [
            (OpenSort::Updated, "sort:updated-desc"),
            (OpenSort::Created, "sort:created-desc"),
        ] {
            assert_eq!(
                mine_search(&repo, "me", sort),
                format!("repo:owner/repo is:pr is:open author:me archived:false {qualifier}")
            );
        }
        assert!(MINE_QUERY.contains("createdAt"), "{MINE_QUERY}");
        assert!(MINE_QUERY.contains("updatedAt"), "{MINE_QUERY}");
    }

    #[test]
    fn mine_query_requests_required_and_individual_approvals() {
        assert!(MINE_QUERY.contains("reviewDecision"), "{MINE_QUERY}");
        assert!(
            MINE_QUERY.contains("latestOpinionatedReviews"),
            "{MINE_QUERY}"
        );
    }

    #[test]
    fn every_pr_query_requests_the_head_branch() {
        assert!(QUEUE_QUERY.contains("headRefName"));
        assert!(MINE_QUERY.contains("headRefName"));
        assert!(merged_query(20).contains("headRefName"));
        assert!(REVIEWS_QUERY.contains("headRefName"));
    }

    #[test]
    fn required_contexts_count_only_required_checks() {
        let mut summary = RequiredSummary::default();
        for context in [
            RequiredContext::CheckRun {
                required: true,
                status: "COMPLETED".to_string(),
                conclusion: Some("FAILURE".to_string()),
                started_at: Some("2026-08-28T11:00:00Z".to_string()),
            },
            RequiredContext::CheckRun {
                required: true,
                status: "IN_PROGRESS".to_string(),
                conclusion: None,
                started_at: Some("2026-08-28T10:00:00Z".to_string()),
            },
            RequiredContext::CheckRun {
                required: false,
                status: "COMPLETED".to_string(),
                conclusion: Some("SUCCESS".to_string()),
                started_at: Some("2026-08-28T09:00:00Z".to_string()),
            },
            RequiredContext::StatusContext {
                required: true,
                state: "SUCCESS".to_string(),
            },
        ] {
            context.add_to(&mut summary);
        }

        assert_eq!(
            summary.checks,
            Checks {
                fail: 1,
                running: 1,
                pass: 1,
            }
        );
        assert_eq!(
            summary.build_started_at.as_deref(),
            Some("2026-08-28T10:00:00Z")
        );
    }

    #[test]
    fn required_query_uses_commit_id_and_pr_number_variables() {
        for (after, expected) in [
            (None, serde_json::Value::Null),
            (Some("CURSOR"), serde_json::Value::from("CURSOR")),
        ] {
            let pending = [PendingRequired {
                target: 0,
                pull_request_number: 42,
                commit_id: "COMMIT_ID".to_string(),
                after: after.map(str::to_string),
            }];
            let (query, variables) = required_query(&pending);

            assert!(query.contains("isRequired(pullRequestNumber: $pull0)"));
            assert!(query.contains("contexts(first: 100, after: $after0)"));
            assert_eq!(variables["commit0"], "COMMIT_ID");
            assert_eq!(variables["pull0"], 42);
            assert_eq!(variables["after0"], expected);
        }
    }
}

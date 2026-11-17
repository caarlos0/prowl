//! Typed serde models for the GitHub GraphQL queries, plus the fetch
//! helpers that run them. Most queries are sent verbatim; the merged query
//! interpolates its page size, and required-check queries batch aliased ref and
//! commit lookups.

use crate::cli::OpenSort;
use crate::github::{Client, GqlError, Repo};
use crate::status::{self, Approval, Checks};
use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

#[derive(Debug)]
pub(crate) struct GraphqlData<T> {
    pub(crate) data: T,
    pub(crate) errors: Vec<GqlError>,
}

// ----------------------------------------------------------------------------
// Merge queue
// ----------------------------------------------------------------------------

pub const QUEUE_QUERY: &str = r#"query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    defaultBranchRef {
      id
      refUpdateRule { requiredStatusCheckContexts }
      rules(first: 100) {
        pageInfo { hasNextPage endCursor }
        nodes {
          parameters {
            __typename
            ... on RequiredStatusChecksParameters {
              requiredStatusChecks { context }
            }
          }
        }
      }
    }
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
          pullRequest { number title url headRefName baseRef { id } author { login } }
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
    #[serde(rename = "defaultBranchRef")]
    default_branch_ref: Option<DefaultBranchRef>,
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
    #[serde(skip)]
    pub(crate) missing_required_checks: u64,
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
    /// the queue's own CI semaphore. RUN includes unreported required checks,
    /// even before the speculative commit exists.
    pub fn checks(&self) -> Checks {
        let rollup = self
            .head_commit
            .as_ref()
            .and_then(|h| h.status_check_rollup.as_ref());
        let mut checks = if let Some(summary) = &self.required {
            summary.checks
        } else if let Some(rollup) = rollup {
            checks_from_counts(
                &rollup.contexts.check_runs,
                &rollup.contexts.status_contexts,
            )
        } else {
            Checks::default()
        };
        checks.running += self.missing_required_checks;
        checks
    }

    fn required_target(&self) -> RequiredTarget {
        RequiredTarget {
            pull_request_number: self.pull_request.number,
            commit_id: self.head_commit.as_ref().map(|commit| commit.id.clone()),
            base_ref_id: self
                .pull_request
                .base_ref
                .as_ref()
                .map(|base| base.id.clone()),
        }
    }

    fn apply_required(&mut self, summary: RequiredSummary, required_only: bool) {
        self.missing_required_checks = summary.missing;
        if required_only {
            self.required = Some(summary);
        }
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
    #[serde(rename = "baseRef")]
    pub(crate) base_ref: Option<BaseRef>,
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
        .map_or_else(Vec::new, |q| q.entries.nodes)
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
    Ok(fetch_queue_with_context_cache(
        client,
        repo,
        required_only,
        &mut RequiredContextCache::default(),
    )?
    .data)
}

pub(crate) fn fetch_queue_with_context_cache(
    client: &Client,
    repo: &Repo,
    required_only: bool,
    context_cache: &mut RequiredContextCache,
) -> Result<GraphqlData<(Vec<QueueEntryNode>, Option<i64>)>> {
    let (mut data, mut errors): (QueueData, _) = client.graphql_with_errors(
        QUEUE_QUERY,
        serde_json::json!({ "owner": repo.owner, "name": repo.name }),
    )?;
    let default_branch_ref = data
        .repository
        .as_mut()
        .and_then(|repository| repository.default_branch_ref.take());
    seed_default_branch_ref(default_branch_ref, context_cache)?;
    if could_affect_default_branch_rules(&errors)
        && let Some(id) = &context_cache.default_branch_ref_id
    {
        context_cache
            .errors
            .entry(id.clone())
            .or_default()
            .extend(errors.iter().cloned());
    }
    let eta = queue_next_eta(&data);
    let mut nodes = queue_nodes(data);
    errors.extend(apply_queue_required(
        client,
        &mut nodes,
        required_only,
        context_cache,
    )?);
    Ok(GraphqlData {
        data: (nodes, eta),
        errors,
    })
}

fn apply_queue_required(
    client: &Client,
    nodes: &mut [QueueEntryNode],
    required_only: bool,
    context_cache: &mut RequiredContextCache,
) -> Result<Vec<GqlError>> {
    let targets: Vec<_> = nodes.iter().map(QueueEntryNode::required_target).collect();
    let (summaries, errors) = fetch_required(client, &targets, required_only, context_cache)?;
    for (node, summary) in nodes.iter_mut().zip(summaries) {
        node.apply_required(summary, required_only);
    }
    Ok(errors)
}

// ----------------------------------------------------------------------------
// My open PRs
// ----------------------------------------------------------------------------

pub const MINE_QUERY: &str = r#"query($owner: String!, $name: String!, $q: String!, $after: String, $includeDefaultBranchRef: Boolean!, $includeQueue: Boolean!) {
  repository(owner: $owner, name: $name) {
    defaultBranchRef @include(if: $includeDefaultBranchRef) {
      id
      refUpdateRule { requiredStatusCheckContexts }
      rules(first: 100) {
        pageInfo { hasNextPage endCursor }
        nodes {
          parameters {
            __typename
            ... on RequiredStatusChecksParameters {
              requiredStatusChecks { context }
            }
          }
        }
      }
    }
    mergeQueue @include(if: $includeQueue) {
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
          pullRequest { number title url headRefName baseRef { id } author { login } }
        }
      }
    }
  }
  search(type: ISSUE, first: 5, after: $after, query: $q) {
    pageInfo { hasNextPage endCursor }
    nodes {
      ... on PullRequest {
        number title url mergeable mergeStateStatus isDraft updatedAt createdAt headRefName
        baseRef { id }
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

const WATCH_MINE_REVIEWS_QUERY: &str = r#"query($owner: String!, $name: String!, $q: String!, $after: String, $includeQueue: Boolean!, $me: String!, $requested: String!, $reviewed: String!, $reviewedMerged: String!, $mineMerged: String!, $mergedLimit: Int!, $includeMineMerged: Boolean!) {
  repository(owner: $owner, name: $name) {
    defaultBranchRef {
      id
      refUpdateRule { requiredStatusCheckContexts }
      rules(first: 100) {
        pageInfo { hasNextPage endCursor }
        nodes {
          parameters {
            __typename
            ... on RequiredStatusChecksParameters {
              requiredStatusChecks { context }
            }
          }
        }
      }
    }
    mergeQueue @include(if: $includeQueue) {
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
          pullRequest { number title url headRefName baseRef { id } author { login } }
        }
      }
    }
  }
  search(type: ISSUE, first: 5, after: $after, query: $q) {
    pageInfo { hasNextPage endCursor }
    nodes {
      ... on PullRequest {
        number title url mergeable mergeStateStatus isDraft updatedAt createdAt headRefName
        baseRef { id }
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
  requested: search(type: ISSUE, first: 50, query: $requested) { nodes { ...watchReview } }
  reviewed: search(type: ISSUE, first: 50, query: $reviewed) { nodes { ...watchReview } }
  reviewedMerged: search(type: ISSUE, first: $mergedLimit, query: $reviewedMerged) { nodes { ...watchMerged } }
  mineMerged: search(type: ISSUE, first: $mergedLimit, query: $mineMerged) @include(if: $includeMineMerged) { nodes { ...watchMerged } }
}
fragment watchReview on PullRequest {
  number title url headRefName isDraft updatedAt
  author { login }
  commits(last: 1) { nodes { commit { committedDate } } }
  reviews(author: $me, first: 20, states: [APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]) { nodes { submittedAt } }
}
fragment watchMerged on PullRequest {
  number title url headRefName mergedAt author { login }
}"#;

#[derive(Debug, Deserialize)]
pub struct MineData {
    repository: Option<DefaultBranchRepository>,
    pub search: MineNodes,
}

#[derive(Debug, Deserialize)]
struct DefaultBranchRepository {
    #[serde(rename = "defaultBranchRef")]
    default_branch_ref: Option<DefaultBranchRef>,
}

#[derive(Debug, Deserialize)]
pub struct MineNodes {
    pub nodes: Vec<PrNode>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

impl MineNodes {
    fn add_to(self, nodes: &mut Vec<PrNode>) -> Result<Option<String>> {
        let after = self.page_info.next_cursor()?;
        nodes.extend(self.nodes);
        Ok(after)
    }
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
    #[serde(rename = "baseRef")]
    pub(crate) base_ref: Option<BaseRef>,
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
    #[serde(skip)]
    pub(crate) missing_required_checks: u64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct BaseRef {
    id: String,
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
    /// RUN also includes required checks that have not reported on that commit.
    pub fn checks(&self) -> Checks {
        let rollup = self
            .commits
            .nodes
            .first()
            .and_then(|n| n.commit.status_check_rollup.as_ref());
        let mut checks = if let Some(checks) = self.required_checks {
            checks
        } else if let Some(rollup) = rollup {
            checks_from_counts(
                &rollup.contexts.check_runs,
                &rollup.contexts.status_contexts,
            )
        } else {
            Checks::default()
        };
        checks.running += self.missing_required_checks;
        checks
    }

    fn apply_required(&mut self, summary: RequiredSummary, required_only: bool) {
        self.missing_required_checks = summary.missing;
        if required_only {
            self.required_checks = Some(summary.checks);
        }
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

pub fn mine_search(repo: &Repo, me: &str, sort: OpenSort, no_draft: bool) -> String {
    let draft = if no_draft { " draft:false" } else { "" };
    format!(
        "repo:{}/{} is:pr is:open author:{}{} archived:false {}",
        repo.owner,
        repo.name,
        me,
        draft,
        sort.qualifier()
    )
}

pub fn fetch_my_prs(
    client: &Client,
    repo: &Repo,
    me: &str,
    sort: OpenSort,
    no_draft: bool,
    required_only: bool,
) -> Result<Vec<PrNode>> {
    Ok(fetch_my_prs_with_context_cache(
        client,
        repo,
        me,
        sort,
        no_draft,
        required_only,
        &mut RequiredContextCache::default(),
    )?
    .data)
}

pub(crate) fn fetch_my_prs_with_context_cache(
    client: &Client,
    repo: &Repo,
    me: &str,
    sort: OpenSort,
    no_draft: bool,
    required_only: bool,
    context_cache: &mut RequiredContextCache,
) -> Result<GraphqlData<Vec<PrNode>>> {
    let q = mine_search(repo, me, sort, no_draft);
    let include_default_branch_ref = context_cache.default_branch_ref_id.is_none();
    let (data, errors): (MineData, _) = client.graphql_with_errors(
        MINE_QUERY,
        serde_json::json!({
            "owner": repo.owner,
            "name": repo.name,
            "q": q,
            "after": null,
            "includeDefaultBranchRef": include_default_branch_ref,
            "includeQueue": false,
        }),
    )?;
    if include_default_branch_ref {
        if could_affect_default_branch_rules(&errors)
            && let Some(branch) = data
                .repository
                .as_ref()
                .and_then(|repository| repository.default_branch_ref.as_ref())
        {
            context_cache
                .errors
                .entry(branch.id.clone())
                .or_default()
                .extend(errors.iter().cloned());
        }
        seed_default_branch_ref(
            data.repository
                .and_then(|repository| repository.default_branch_ref),
            context_cache,
        )?;
    }
    fetch_mine_pages(
        client,
        repo,
        &q,
        required_only,
        context_cache,
        GraphqlData {
            data: data.search,
            errors,
        },
    )
}

pub(crate) struct MineQueueResults {
    pub mine: Result<GraphqlData<Vec<PrNode>>>,
    pub queue: Result<GraphqlData<(Vec<QueueEntryNode>, Option<i64>)>>,
}

struct MineQueueOptions<'a> {
    q: &'a str,
    required_only: bool,
    include_queue: bool,
}

pub(crate) fn fetch_mine_and_queue_with_context_cache(
    client: &Client,
    repo: &Repo,
    me: &str,
    sort: OpenSort,
    no_draft: bool,
    required_only: bool,
    context_cache: &mut RequiredContextCache,
) -> Result<MineQueueResults> {
    let q = mine_search(repo, me, sort, no_draft);
    let (data, errors) = client.graphql_with_errors::<serde_json::Value>(
        MINE_QUERY,
        serde_json::json!({
            "owner": repo.owner,
            "name": repo.name,
            "q": q,
            "after": null,
            "includeDefaultBranchRef": true,
            "includeQueue": true,
        }),
    )?;
    parse_mine_queue_response(
        client,
        repo,
        context_cache,
        &data,
        errors,
        MineQueueOptions {
            q: &q,
            required_only,
            include_queue: true,
        },
    )
}

pub(crate) struct WatchQueryOptions<'a> {
    pub(crate) requested_qualifier: &'a str,
    pub(crate) since: &'a str,
    pub(crate) merged_limit: usize,
    pub(crate) no_draft: bool,
    pub(crate) include_queue: bool,
    pub(crate) include_mine_merged: bool,
}

pub(crate) struct MineReviewsQueueResults {
    pub(crate) mine_queue: Result<MineQueueResults>,
    pub(crate) reviews: ReviewsAndMerged,
}

pub(crate) fn fetch_mine_queue_reviews_with_context_cache(
    client: &Client,
    repo: &Repo,
    me: &str,
    sort: OpenSort,
    required_only: bool,
    context_cache: &mut RequiredContextCache,
    options: WatchQueryOptions<'_>,
) -> Result<MineReviewsQueueResults> {
    let q = mine_search(repo, me, sort, options.no_draft);
    let (requested, reviewed) = reviews_searches(repo, me, options.requested_qualifier);
    let (data, errors) = client.graphql_with_errors::<serde_json::Value>(
        WATCH_MINE_REVIEWS_QUERY,
        serde_json::json!({
            "owner": repo.owner,
            "name": repo.name,
            "q": q,
            "after": null,
            "includeQueue": options.include_queue,
            "me": me,
            "requested": requested,
            "reviewed": reviewed,
            "reviewedMerged": reviewed_merged_search(repo, me, options.since),
            "mineMerged": merged_search(repo, me, options.since),
            "mergedLimit": options.merged_limit.clamp(1, 100),
            "includeMineMerged": options.include_mine_merged,
        }),
    )?;
    let mine_errors = errors
        .iter()
        .filter(|error| {
            !matches!(
                error
                    .path
                    .as_ref()
                    .and_then(|path| path.first())
                    .and_then(serde_json::Value::as_str),
                Some("requested" | "reviewed" | "mineMerged" | "reviewedMerged")
            )
        })
        .cloned()
        .collect();
    let review_errors = errors
        .iter()
        .filter(|error| {
            !matches!(
                error
                    .path
                    .as_ref()
                    .and_then(|path| path.first())
                    .and_then(serde_json::Value::as_str),
                Some("search" | "repository")
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    let mine_queue = parse_mine_queue_response(
        client,
        repo,
        context_cache,
        &data,
        mine_errors,
        MineQueueOptions {
            q: &q,
            required_only,
            include_queue: options.include_queue,
        },
    );
    let reviews = parse_reviews_and_merged(&data, &review_errors, options.include_mine_merged);
    Ok(MineReviewsQueueResults {
        mine_queue,
        reviews,
    })
}

fn parse_mine_queue_response(
    client: &Client,
    repo: &Repo,
    context_cache: &mut RequiredContextCache,
    data: &serde_json::Value,
    errors: Vec<GqlError>,
    options: MineQueueOptions<'_>,
) -> Result<MineQueueResults> {
    let MineQueueOptions {
        q,
        required_only,
        include_queue,
    } = options;
    let mine_error = mine_queue_graphql_error(&errors, false);
    let queue_error = mine_queue_graphql_error(&errors, true);

    let repository = data.get("repository").filter(|value| !value.is_null());
    let default_branch_ref = repository
        .and_then(|repository| repository.get("defaultBranchRef"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let default_branch_ref: Option<DefaultBranchRef> =
        serde_json::from_value(default_branch_ref).context("parsing default branch rules")?;
    seed_default_branch_ref(default_branch_ref, context_cache)?;
    if could_affect_default_branch_rules(&errors)
        && let Some(id) = &context_cache.default_branch_ref_id
    {
        let rule_errors = errors
            .iter()
            .filter(|error| could_affect_default_branch_rules(std::slice::from_ref(*error)))
            .cloned()
            .collect::<Vec<_>>();
        context_cache
            .errors
            .entry(id.clone())
            .or_default()
            .extend(rule_errors);
    }

    let queue = if include_queue {
        let merge_queue = repository
            .and_then(|repository| repository.get("mergeQueue"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        match serde_json::from_value::<Option<MergeQueue>>(merge_queue)
            .context("parsing merge queue")
        {
            Ok(Some(merge_queue)) => {
                let eta = merge_queue.next_entry_estimated_time_to_merge;
                let mut nodes = merge_queue.entries.nodes;
                let mut warnings = mine_queue_graphql_warnings(&errors, true);
                apply_queue_required(client, &mut nodes, required_only, context_cache).map(
                    |required_warnings| {
                        warnings.extend(required_warnings);
                        GraphqlData {
                            data: (nodes, eta),
                            errors: warnings,
                        }
                    },
                )
            }
            Ok(None) => queue_error.map_or_else(
                || {
                    Ok(GraphqlData {
                        data: (Vec::new(), None),
                        errors: mine_queue_graphql_warnings(&errors, true),
                    })
                },
                Err,
            ),
            Err(parse_error) => Err(queue_error.unwrap_or(parse_error)),
        }
    } else {
        Ok(GraphqlData {
            data: (Vec::new(), None),
            errors: Vec::new(),
        })
    };
    let first_page = data
        .get("search")
        .cloned()
        .context("GraphQL response omitted My PRs search")
        .and_then(|search| {
            serde_json::from_value::<MineNodes>(search).context("parsing My PRs search")
        });
    let mine = match first_page {
        Ok(first_page) => fetch_mine_pages(
            client,
            repo,
            q,
            required_only,
            context_cache,
            GraphqlData {
                data: first_page,
                errors: mine_queue_graphql_warnings(&errors, false),
            },
        ),
        Err(parse_error) => Err(mine_error.unwrap_or(parse_error)),
    };
    Ok(MineQueueResults { mine, queue })
}

fn mine_queue_graphql_warnings(errors: &[GqlError], queue: bool) -> Vec<GqlError> {
    errors
        .iter()
        .filter(|error| {
            let path = error.path.as_deref().unwrap_or_default();
            let root = path.first().and_then(serde_json::Value::as_str);
            let search_error = root == Some("search");
            let queue_error = root == Some("repository")
                && path.get(1).and_then(serde_json::Value::as_str) == Some("mergeQueue");
            !(if queue { search_error } else { queue_error })
        })
        .cloned()
        .collect()
}

fn mine_queue_graphql_error(errors: &[GqlError], queue: bool) -> Option<anyhow::Error> {
    let messages = errors
        .iter()
        .filter(|error| {
            let path = error.path.as_deref().unwrap_or_default();
            match path.first().and_then(serde_json::Value::as_str) {
                Some("search") => !queue,
                Some("repository")
                    if path.get(1).and_then(serde_json::Value::as_str) == Some("mergeQueue") =>
                {
                    queue
                }
                Some("repository")
                    if path.get(1).and_then(serde_json::Value::as_str)
                        == Some("defaultBranchRef") =>
                {
                    false
                }
                _ => true,
            }
        })
        .map(|error| error.message.as_str())
        .collect::<Vec<_>>();
    (!messages.is_empty()).then(|| anyhow!("GraphQL error: {}", messages.join("; ")))
}

fn fetch_mine_pages(
    client: &Client,
    repo: &Repo,
    q: &str,
    required_only: bool,
    context_cache: &mut RequiredContextCache,
    first_page: GraphqlData<MineNodes>,
) -> Result<GraphqlData<Vec<PrNode>>> {
    let GraphqlData {
        data: first_page,
        mut errors,
    } = first_page;
    let mut nodes = Vec::new();
    let mut after = first_page.add_to(&mut nodes)?;
    while let Some(cursor) = after {
        let (data, page_errors): (MineData, _) = client.graphql_with_errors(
            MINE_QUERY,
            serde_json::json!({
                "owner": repo.owner,
                "name": repo.name,
                "q": q,
                "after": cursor,
                "includeDefaultBranchRef": false,
                "includeQueue": false,
            }),
        )?;
        errors.extend(page_errors);
        after = data.search.add_to(&mut nodes)?;
    }
    let targets: Vec<_> = nodes
        .iter()
        .map(|node| RequiredTarget {
            pull_request_number: node.number,
            commit_id: node
                .commits
                .nodes
                .first()
                .map(|commit| commit.commit.id.clone()),
            base_ref_id: node.base_ref.as_ref().map(|base| base.id.clone()),
        })
        .collect();
    let (summaries, required_errors) =
        fetch_required(client, &targets, required_only, context_cache)?;
    errors.extend(required_errors);
    for (node, summary) in nodes.iter_mut().zip(summaries) {
        node.apply_required(summary, required_only);
    }
    Ok(GraphqlData {
        data: nodes,
        errors,
    })
}

// ----------------------------------------------------------------------------
// Required status checks
// ----------------------------------------------------------------------------

const REQUIRED_CONTEXTS_BATCH_SIZE: usize = 5;

#[derive(Debug, Deserialize)]
struct RequiredRefsData {
    #[serde(flatten)]
    refs: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct RequiredRef {
    #[serde(rename = "refUpdateRule")]
    update_rule: Option<RefUpdateRule>,
    rules: RefRules,
}

#[derive(Debug, Deserialize)]
struct DefaultBranchRef {
    id: String,
    #[serde(flatten)]
    required: RequiredRef,
}

#[derive(Debug, Deserialize)]
struct RefUpdateRule {
    #[serde(rename = "requiredStatusCheckContexts")]
    contexts: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct RefRules {
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
    nodes: Vec<RefRule>,
}

#[derive(Debug, Deserialize)]
struct RefRule {
    parameters: Option<RuleParameters>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum RuleParameters {
    RequiredStatusChecksParameters {
        #[serde(rename = "requiredStatusChecks")]
        checks: Vec<RequiredCheck>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct RequiredCheck {
    context: String,
}

impl RequiredRef {
    fn add_to(self, contexts: &mut HashSet<String>) -> Result<Option<String>> {
        if let Some(rule) = self.update_rule {
            contexts.extend(rule.contexts.into_iter().flatten());
        }
        for rule in self.rules.nodes {
            if let Some(RuleParameters::RequiredStatusChecksParameters { checks }) = rule.parameters
            {
                contexts.extend(checks.into_iter().map(|check| check.context));
            }
        }
        self.rules.page_info.next_cursor()
    }
}

#[derive(Debug)]
struct RequiredTarget {
    pull_request_number: i64,
    commit_id: Option<String>,
    base_ref_id: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct RequiredContextCache {
    contexts: HashMap<String, HashSet<String>>,
    errors: HashMap<String, Vec<GqlError>>,
    pending: HashMap<String, String>,
    default_branch_ref_id: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct RequiredSummary {
    pub(crate) checks: Checks,
    pub(crate) build_started_at: Option<String>,
    pub(crate) contexts: HashSet<String>,
    pub(crate) missing: u64,
}

impl RequiredSummary {
    fn include_missing(&mut self, expected: &HashSet<String>) {
        self.missing = expected.difference(&self.contexts).count() as u64;
    }

    fn add_page(&mut self, commit: RequiredCommit) -> Result<Option<String>> {
        let Some(rollup) = commit.status_check_rollup else {
            return Ok(None);
        };
        for context in rollup.contexts.nodes {
            context.add_to(self);
        }
        rollup.contexts.page_info.next_cursor()
    }
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
    page_info: PageInfo,
    nodes: Vec<RequiredContext>,
}

#[derive(Debug, Deserialize)]
struct PageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

impl PageInfo {
    fn next_cursor(self) -> Result<Option<String>> {
        if self.has_next_page {
            Ok(Some(
                self.end_cursor.context("GraphQL page had no end cursor")?,
            ))
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum RequiredContext {
    CheckRun {
        required: bool,
        name: String,
        status: String,
        conclusion: Option<String>,
        #[serde(rename = "startedAt")]
        started_at: Option<String>,
    },
    StatusContext {
        required: bool,
        context: String,
        state: String,
    },
}

impl RequiredContext {
    fn add_to(self, summary: &mut RequiredSummary) {
        match self {
            RequiredContext::CheckRun {
                required: true,
                name,
                status,
                conclusion,
                started_at,
            } => {
                summary.contexts.insert(name);
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
                context,
                state,
            } => {
                summary.contexts.insert(context);
                summary.checks.add(status::status_context_lamp(&state), 1);
            }
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

#[derive(Debug)]
struct PendingRequiredRules {
    id: String,
    after: Option<String>,
    contexts: HashSet<String>,
}

fn required_rules_query(pending: &[PendingRequiredRules]) -> (String, serde_json::Value) {
    let mut declarations = Vec::with_capacity(pending.len() * 2);
    let mut fields = String::new();
    let mut variables = serde_json::Map::new();
    for (alias, item) in pending.iter().enumerate() {
        declarations.extend([format!("$id{alias}: ID!"), format!("$after{alias}: String")]);
        write!(
            fields,
            r#"r{alias}: node(id: $id{alias}) {{
  ... on Ref {{
    refUpdateRule {{ requiredStatusCheckContexts }}
    rules(first: 100, after: $after{alias}) {{
      pageInfo {{ hasNextPage endCursor }}
      nodes {{
        parameters {{
          __typename
          ... on RequiredStatusChecksParameters {{
            requiredStatusChecks {{ context }}
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
            format!("id{alias}"),
            serde_json::Value::String(item.id.clone()),
        );
        variables.insert(format!("after{alias}"), serde_json::json!(item.after));
    }
    (
        format!("query({}) {{\n{fields}}}", declarations.join(", ")),
        serde_json::Value::Object(variables),
    )
}

fn fetch_required_contexts(
    ids: &[String],
    context_cache: &mut RequiredContextCache,
    mut fetch: impl FnMut(&str, serde_json::Value) -> Result<(RequiredRefsData, Vec<GqlError>)>,
) -> Result<Vec<GqlError>> {
    let mut errors = Vec::new();
    let mut errors_by_ref = HashMap::<String, Vec<GqlError>>::new();
    let mut seen = HashSet::new();
    let mut pending: Vec<_> = ids
        .iter()
        .filter(|id| {
            (!context_cache.contexts.contains_key(*id) || context_cache.pending.contains_key(*id))
                && seen.insert(id.as_str())
        })
        .map(|id| PendingRequiredRules {
            id: id.clone(),
            after: context_cache.pending.get(id).cloned(),
            contexts: context_cache.contexts.get(id).cloned().unwrap_or_default(),
        })
        .collect();

    while !pending.is_empty() {
        for batch in pending.chunks_mut(REQUIRED_CONTEXTS_BATCH_SIZE) {
            let (query, variables) = required_rules_query(batch);
            let (mut data, batch_errors) = fetch(&query, variables)?;
            errors.extend(batch_errors.iter().cloned());
            for (index, item) in batch.iter().enumerate() {
                let relevant = batch_errors
                    .iter()
                    .filter(|error| {
                        error
                            .path
                            .as_ref()
                            .and_then(|path| path.first())
                            .and_then(serde_json::Value::as_str)
                            .and_then(|root| root.strip_prefix('r'))
                            .and_then(|alias| alias.parse::<usize>().ok())
                            .is_none_or(|alias| alias == index)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if !relevant.is_empty() {
                    errors_by_ref
                        .entry(item.id.clone())
                        .or_default()
                        .extend(relevant);
                }
            }
            for (alias, item) in batch.iter_mut().enumerate() {
                let value = data
                    .refs
                    .remove(&format!("r{alias}"))
                    .with_context(|| format!("required-rule response omitted r{alias}"))?;
                let rule: Option<RequiredRef> = serde_json::from_value(value)
                    .with_context(|| format!("could not parse required-rule response r{alias}"))?;
                let rule = rule.with_context(|| {
                    format!("required-check base branch {} was unavailable", item.id)
                })?;
                item.after = rule.add_to(&mut item.contexts)?;
            }
        }
        pending.retain_mut(|item| {
            if item.after.is_some() {
                true
            } else {
                context_cache
                    .contexts
                    .insert(item.id.clone(), std::mem::take(&mut item.contexts));
                context_cache.pending.remove(&item.id);
                if let Some(errors) = errors_by_ref.remove(&item.id) {
                    context_cache
                        .errors
                        .entry(item.id.clone())
                        .or_default()
                        .extend(errors);
                }
                false
            }
        });
    }
    Ok(errors)
}

fn seed_default_branch_ref(
    branch: Option<DefaultBranchRef>,
    context_cache: &mut RequiredContextCache,
) -> Result<()> {
    let Some(branch) = branch else {
        return Ok(());
    };
    if !context_cache.contexts.contains_key(&branch.id) {
        let mut contexts = HashSet::new();
        if let Some(after) = branch.required.add_to(&mut contexts)? {
            context_cache.pending.insert(branch.id.clone(), after);
        }
        context_cache.contexts.insert(branch.id.clone(), contexts);
    }
    context_cache.default_branch_ref_id = Some(branch.id);
    Ok(())
}

fn could_affect_default_branch_rules(errors: &[GqlError]) -> bool {
    errors.iter().any(|error| {
        let Some(path) = &error.path else {
            return true;
        };
        match path.as_slice() {
            [root, field, ..]
                if root.as_str() == Some("repository")
                    && field.as_str() == Some("defaultBranchRef") =>
            {
                true
            }
            [root, ..] if matches!(root.as_str(), Some("repository" | "search")) => false,
            _ => true,
        }
    })
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
            name status conclusion startedAt
          }}
          ... on StatusContext {{
            required: isRequired(pullRequestNumber: $pull{alias})
            context state
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

fn fetch_required_pages(
    mut pending: Vec<PendingRequired>,
    summaries: &mut [RequiredSummary],
    mut fetch: impl FnMut(&str, serde_json::Value) -> Result<HashMap<String, Option<RequiredCommit>>>,
) -> Result<()> {
    while !pending.is_empty() {
        for batch in pending.chunks_mut(5) {
            let (query, variables) = required_query(batch);
            let mut data = fetch(&query, variables)?;
            for (alias, item) in batch.iter_mut().enumerate() {
                let commit = data
                    .remove(&format!("c{alias}"))
                    .with_context(|| format!("required-check response omitted c{alias}"))?
                    .with_context(|| format!("required-check commit c{alias} was unavailable"))?;
                item.after = summaries[item.target].add_page(commit)?;
            }
        }
        pending.retain(|item| item.after.is_some());
    }
    Ok(())
}

fn fetch_required(
    client: &Client,
    targets: &[RequiredTarget],
    required_only: bool,
    context_cache: &mut RequiredContextCache,
) -> Result<(Vec<RequiredSummary>, Vec<GqlError>)> {
    let base_ref_ids: Vec<_> = targets
        .iter()
        .filter_map(|target| target.base_ref_id.as_ref())
        .cloned()
        .collect();
    let mut errors = fetch_required_contexts(&base_ref_ids, context_cache, |query, variables| {
        client.graphql_with_errors(query, variables)
    })?;
    let empty = HashSet::new();
    let expected = |target: &RequiredTarget| {
        target
            .base_ref_id
            .as_ref()
            .and_then(|id| context_cache.contexts.get(id.as_str()))
            .unwrap_or(&empty)
    };
    let mut summaries: Vec<_> = (0..targets.len())
        .map(|_| RequiredSummary::default())
        .collect();
    let pending: Vec<_> = targets
        .iter()
        .enumerate()
        .filter(|(_, item)| required_only || !expected(item).is_empty())
        .filter_map(|(target, item)| {
            item.commit_id.as_ref().map(|id| PendingRequired {
                target,
                pull_request_number: item.pull_request_number,
                commit_id: id.clone(),
                after: None,
            })
        })
        .collect();

    fetch_required_pages(pending, &mut summaries, |query, variables| {
        let (data, page_errors) = client.graphql_with_errors(query, variables)?;
        errors.extend(page_errors);
        Ok(data)
    })?;

    for (target, summary) in targets.iter().zip(&mut summaries) {
        summary.include_missing(expected(target));
        if let Some(ref_id) = &target.base_ref_id
            && let Some(ref_errors) = context_cache.errors.get(ref_id)
        {
            errors.extend(ref_errors.iter().cloned());
        }
    }
    Ok((summaries, errors))
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
    Ok(fetch_merged_with_errors(client, repo, me, since, limit)?.data)
}

pub(crate) fn fetch_merged_with_errors(
    client: &Client,
    repo: &Repo,
    me: &str,
    since: &str,
    limit: usize,
) -> Result<GraphqlData<Vec<MergedNode>>> {
    let q = merged_search(repo, me, since);
    let (data, errors): (MergedData, _) =
        client.graphql_with_errors(&merged_query(limit), serde_json::json!({ "q": q }))?;
    Ok(GraphqlData {
        data: data.search.nodes,
        errors,
    })
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
    Ok(fetch_reviewed_merged_with_errors(client, repo, me, since, limit)?.data)
}

pub(crate) fn fetch_reviewed_merged_with_errors(
    client: &Client,
    repo: &Repo,
    me: &str,
    since: &str,
    limit: usize,
) -> Result<GraphqlData<Vec<MergedNode>>> {
    let q = reviewed_merged_search(repo, me, since);
    let (data, errors): (MergedData, _) =
        client.graphql_with_errors(&merged_query(limit), serde_json::json!({ "q": q }))?;
    Ok(GraphqlData {
        data: data.search.nodes,
        errors,
    })
}

fn merged_alias(
    data: &serde_json::Value,
    errors: &[GqlError],
    alias: &str,
) -> Result<GraphqlData<Vec<MergedNode>>> {
    let errors = alias_errors(errors, &[alias]);
    let search = data
        .get(alias)
        .filter(|search| !search.is_null())
        .ok_or_else(|| {
            if errors.is_empty() {
                anyhow!("GraphQL response omitted `{alias}` merged search")
            } else {
                anyhow!(
                    "GraphQL error: {}",
                    errors
                        .iter()
                        .map(|error| error.message.as_str())
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            }
        })?;
    let search: MergedNodes = serde_json::from_value(search.clone())
        .with_context(|| format!("parsing `{alias}` merged search"))?;
    Ok(GraphqlData {
        data: search.nodes,
        errors,
    })
}

fn alias_errors(errors: &[GqlError], aliases: &[&str]) -> Vec<GqlError> {
    const KNOWN_ALIASES: &[&str] = &["requested", "reviewed", "mineMerged", "reviewedMerged"];
    errors
        .iter()
        .filter(|error| {
            error
                .path
                .as_ref()
                .and_then(|path| path.first())
                .and_then(serde_json::Value::as_str)
                .is_none_or(|root| aliases.contains(&root) || !KNOWN_ALIASES.contains(&root))
        })
        .cloned()
        .collect()
}

// ----------------------------------------------------------------------------
// Reviews (open PRs awaiting / under my review)
// ----------------------------------------------------------------------------

/// Standalone Reviews and merged searches when the combined watch query is not selected.
pub const REVIEWS_QUERY: &str = r#"query($me: String!, $requested: String!, $reviewed: String!, $reviewedMerged: String!, $mineMerged: String!, $mergedLimit: Int!, $includeMineMerged: Boolean!) {
  requested: search(type: ISSUE, first: 50, query: $requested) { nodes { ...rev } }
  reviewed: search(type: ISSUE, first: 50, query: $reviewed) { nodes { ...rev } }
  reviewedMerged: search(type: ISSUE, first: $mergedLimit, query: $reviewedMerged) { nodes { ...merged } }
  mineMerged: search(type: ISSUE, first: $mergedLimit, query: $mineMerged) @include(if: $includeMineMerged) { nodes { ...merged } }
}
fragment rev on PullRequest {
  number title url headRefName isDraft updatedAt
  author { login }
  commits(last: 1) { nodes { commit { committedDate } } }
  reviews(author: $me, first: 20, states: [APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]) { nodes { submittedAt } }
}
fragment merged on PullRequest {
  number title url headRefName mergedAt author { login }
}"#;

const REVIEWS_ONLY_QUERY: &str = r#"query($me: String!, $requested: String!, $reviewed: String!) {
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
    Ok(fetch_reviews_with_errors(client, repo, me, requested_qualifier)?.data)
}

pub(crate) fn fetch_reviews_with_errors(
    client: &Client,
    repo: &Repo,
    me: &str,
    requested_qualifier: &str,
) -> Result<GraphqlData<ReviewsData>> {
    let (requested, reviewed) = reviews_searches(repo, me, requested_qualifier);
    let (data, errors) = client.graphql_with_errors(
        REVIEWS_ONLY_QUERY,
        serde_json::json!({ "me": me, "requested": requested, "reviewed": reviewed }),
    )?;
    Ok(GraphqlData { data, errors })
}

pub(crate) struct ReviewsAndMerged {
    pub(crate) reviews: Result<GraphqlData<ReviewsData>>,
    pub(crate) mine_merged: Option<Result<GraphqlData<Vec<MergedNode>>>>,
    pub(crate) reviewed_merged: Result<GraphqlData<Vec<MergedNode>>>,
}

pub(crate) fn fetch_reviews_and_merged(
    client: &Client,
    repo: &Repo,
    me: &str,
    requested_qualifier: &str,
    since: &str,
    limit: usize,
    include_mine: bool,
) -> Result<ReviewsAndMerged> {
    let (requested, reviewed) = reviews_searches(repo, me, requested_qualifier);
    let (data, errors) = client.graphql_with_errors::<serde_json::Value>(
        REVIEWS_QUERY,
        serde_json::json!({
            "me": me,
            "requested": requested,
            "reviewed": reviewed,
            "reviewedMerged": reviewed_merged_search(repo, me, since),
            "mineMerged": merged_search(repo, me, since),
            "mergedLimit": limit.clamp(1, 100),
            "includeMineMerged": include_mine,
        }),
    )?;
    Ok(parse_reviews_and_merged(&data, &errors, include_mine))
}

fn parse_reviews_and_merged(
    data: &serde_json::Value,
    errors: &[GqlError],
    include_mine: bool,
) -> ReviewsAndMerged {
    let review_errors = alias_errors(errors, &["requested", "reviewed"]);
    let reviews = match serde_json::from_value(data.clone()) {
        Ok(data) => Ok(GraphqlData {
            data,
            errors: review_errors.clone(),
        }),
        Err(_) if !review_errors.is_empty() => Err(anyhow!(
            "GraphQL error: {}",
            review_errors
                .iter()
                .map(|error| error.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        )),
        Err(error) => Err(error).context("parsing Reviews searches"),
    };
    ReviewsAndMerged {
        reviews,
        mine_merged: include_mine.then(|| merged_alias(data, errors, "mineMerged")),
        reviewed_merged: merged_alias(data, errors, "reviewedMerged"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mine_query_uses_small_pages() {
        assert!(
            MINE_QUERY.contains("first: 5, after: $after"),
            "{MINE_QUERY}"
        );
        assert!(MINE_QUERY.contains("$after: String"), "{MINE_QUERY}");
        assert!(
            MINE_QUERY.contains("pageInfo { hasNextPage endCursor }"),
            "{MINE_QUERY}"
        );
    }

    #[test]
    fn watch_query_includes_the_mine_merged_search_once() {
        assert_eq!(
            WATCH_MINE_REVIEWS_QUERY
                .matches("mineMerged: search")
                .count(),
            1,
            "{WATCH_MINE_REVIEWS_QUERY}"
        );
    }

    #[test]
    fn mine_pages_preserve_all_prs_and_return_the_next_cursor() {
        let mut first: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/mine.json")).unwrap();
        first["data"]["search"]["pageInfo"] = serde_json::json!({
            "hasNextPage": true,
            "endCursor": "NEXT",
        });
        let first: MineData =
            crate::github::parse_graphql(&serde_json::to_vec(&first).unwrap()).unwrap();
        let last: MineData =
            crate::github::parse_graphql(include_bytes!("../tests/fixtures/mine_no_rollup.json"))
                .unwrap();
        let mut nodes = Vec::new();
        assert_eq!(
            first.search.add_to(&mut nodes).unwrap().as_deref(),
            Some("NEXT")
        );
        assert_eq!(last.search.add_to(&mut nodes).unwrap(), None);
        assert_eq!(
            nodes.iter().map(|node| node.number).collect::<Vec<_>>(),
            [6475, 5323, 6656, 123]
        );
        assert_eq!(nodes[0].checks().fail, 4);
        assert_eq!(nodes[3].review_threads.unresolved(), (1, false));
    }

    #[test]
    fn mine_empty_page_finishes_pagination() {
        let data: MineData =
            crate::github::parse_graphql(include_bytes!("../tests/fixtures/mine_empty.json"))
                .unwrap();
        let mut nodes = Vec::new();
        assert_eq!(data.search.add_to(&mut nodes).unwrap(), None);
        assert!(nodes.is_empty());
    }

    #[test]
    fn mine_pages_do_not_stop_at_fifty_prs() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/mine_no_rollup.json")).unwrap();
        let mut nodes = Vec::new();
        for page in 0..11 {
            let mut response = fixture.clone();
            response["data"]["search"]["nodes"] = (0..5)
                .map(|offset| {
                    let mut node = fixture["data"]["search"]["nodes"][0].clone();
                    node["number"] = (page * 5 + offset + 1).into();
                    node
                })
                .collect();
            response["data"]["search"]["pageInfo"] = serde_json::json!({
                "hasNextPage": page < 10,
                "endCursor": format!("PAGE_{page}"),
            });
            let data: MineData =
                crate::github::parse_graphql(&serde_json::to_vec(&response).unwrap()).unwrap();
            let after = data.search.add_to(&mut nodes).unwrap();
            if page < 10 {
                assert_eq!(after, Some(format!("PAGE_{page}")));
            } else {
                assert_eq!(after, None);
            }
        }
        assert_eq!(
            nodes.iter().map(|node| node.number).collect::<Vec<_>>(),
            (1..=55).collect::<Vec<_>>()
        );
    }

    #[test]
    fn mine_rejects_a_page_without_pagination_metadata() {
        let mut fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/mine.json")).unwrap();
        fixture["data"]["search"]
            .as_object_mut()
            .unwrap()
            .remove("pageInfo");
        let err = crate::github::parse_graphql::<MineData>(&serde_json::to_vec(&fixture).unwrap())
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("missing field `pageInfo`"),
            "{err:#}"
        );
    }

    #[test]
    fn mine_search_sorts_before_the_api_page_limit() {
        let repo = Repo::parse("owner/repo").unwrap();
        for (sort, qualifier) in [
            (OpenSort::Updated, "sort:updated-desc"),
            (OpenSort::Created, "sort:created-desc"),
        ] {
            assert_eq!(
                mine_search(&repo, "me", sort, false),
                format!("repo:owner/repo is:pr is:open author:me archived:false {qualifier}")
            );
            assert_eq!(
                mine_search(&repo, "me", sort, true),
                format!(
                    "repo:owner/repo is:pr is:open author:me draft:false archived:false {qualifier}"
                )
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
        assert!(
            MINE_QUERY.contains("mergeQueue @include(if: $includeQueue)"),
            "{MINE_QUERY}"
        );
        assert!(
            MINE_QUERY.contains("      }\n    }\n    mergeQueue @include(if: $includeQueue)"),
            "{MINE_QUERY}"
        );
    }

    #[test]
    fn every_pr_query_requests_the_head_branch() {
        assert!(QUEUE_QUERY.contains("headRefName"));
        assert!(QUEUE_QUERY.contains("defaultBranchRef"));
        assert!(MINE_QUERY.contains("headRefName"));
        assert!(MINE_QUERY.contains("defaultBranchRef @include(if: $includeDefaultBranchRef)"));
        assert!(merged_query(20).contains("headRefName"));
        assert!(REVIEWS_QUERY.contains("headRefName"));
    }

    #[test]
    fn reviews_query_aliases_merged_searches() {
        let repo = Repo::parse("o/r").unwrap();
        assert_eq!(
            merged_search(&repo, "me", "2026-01-01"),
            "repo:o/r is:pr is:merged author:me merged:>=2026-01-01 sort:updated-desc"
        );
        assert_eq!(
            reviewed_merged_search(&repo, "me", "2026-01-01"),
            "repo:o/r is:pr is:merged reviewed-by:me -author:me merged:>=2026-01-01 sort:updated-desc"
        );
        assert!(REVIEWS_QUERY.contains("reviewedMerged: search"));
        assert!(REVIEWS_QUERY.contains("mineMerged: search"));
        assert!(REVIEWS_QUERY.contains("@include(if: $includeMineMerged)"));
        assert!(REVIEWS_QUERY.contains("first: $mergedLimit"));
    }

    #[test]
    fn reviews_merged_fetch_keeps_successful_aliases_on_partial_error() {
        let response = serde_json::json!({
            "data": {
                "requested": { "nodes": [] },
                "reviewed": { "nodes": [] },
                "mineMerged": null,
                "reviewedMerged": {
                    "nodes": [{
                        "number": 42,
                        "title": "reviewed",
                        "url": "https://github.com/o/r/pull/42",
                        "headRefName": "branch",
                        "mergedAt": "2026-01-01T00:00:00Z",
                        "author": { "login": "author" }
                    }]
                }
            },
            "errors": [{
                "message": "Mine search unavailable",
                "path": ["mineMerged"]
            }]
        });
        let (client, fake) =
            crate::github::fake::client([("POST /graphql".to_string(), response.to_string())]);
        let fetched = fetch_reviews_and_merged(
            &client,
            &Repo::parse("o/r").unwrap(),
            "me",
            "review-requested",
            "2026-01-01",
            20,
            true,
        )
        .unwrap();

        assert!(fetched.reviews.is_ok());
        assert_eq!(
            fetched.mine_merged.unwrap().unwrap_err().to_string(),
            "GraphQL error: Mine search unavailable"
        );
        assert_eq!(fetched.reviewed_merged.unwrap().data[0].number, 42);
        assert_eq!(fake.requests(), ["POST /graphql"]);
    }

    #[test]
    fn reviews_merged_fetch_preserves_partial_alias_data() {
        let response = serde_json::json!({
            "data": {
                "requested": {
                    "nodes": [{
                        "number": 1,
                        "title": "review",
                        "url": "https://github.com/o/r/pull/1",
                        "headRefName": "branch",
                        "isDraft": false,
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "author": { "login": "author" },
                        "commits": { "nodes": [{ "commit": { "committedDate": "2026-01-01T00:00:00Z" } }] },
                        "reviews": { "nodes": [] }
                    }]
                },
                "reviewed": { "nodes": [] },
                "mineMerged": {
                    "nodes": [{
                        "number": 41,
                        "title": "mine",
                        "url": "https://github.com/o/r/pull/41",
                        "headRefName": "branch",
                        "mergedAt": "2026-01-01T00:00:00Z",
                        "author": { "login": "me" }
                    }]
                },
                "reviewedMerged": {
                    "nodes": [{
                        "number": 42,
                        "title": "reviewed",
                        "url": "https://github.com/o/r/pull/42",
                        "headRefName": "branch",
                        "mergedAt": "2026-01-01T00:00:00Z",
                        "author": { "login": "author" }
                    }]
                }
            },
            "errors": [
                {
                    "message": "Review author unavailable",
                    "path": ["requested", "nodes", 0, "author"]
                },
                {
                    "message": "Mine author unavailable",
                    "path": ["mineMerged", "nodes", 0, "author"]
                }
            ]
        });
        let (client, fake) =
            crate::github::fake::client([("POST /graphql".to_string(), response.to_string())]);
        let fetched = fetch_reviews_and_merged(
            &client,
            &Repo::parse("o/r").unwrap(),
            "me",
            "review-requested",
            "2026-01-01",
            20,
            true,
        )
        .unwrap();

        let reviews = fetched.reviews.unwrap();
        assert_eq!(reviews.data.requested.nodes[0].number, 1);
        assert_eq!(reviews.errors[0].message, "Review author unavailable");
        let mine = fetched.mine_merged.unwrap().unwrap();
        assert_eq!(mine.data[0].number, 41);
        assert_eq!(mine.errors[0].message, "Mine author unavailable");
        let reviewed = fetched.reviewed_merged.unwrap();
        assert_eq!(reviewed.data[0].number, 42);
        assert!(reviewed.errors.is_empty(), "{:?}", reviewed.errors);
        assert_eq!(fake.requests(), ["POST /graphql"]);
    }

    #[test]
    fn required_contexts_count_only_required_checks() {
        let mut summary = RequiredSummary::default();
        for context in [
            RequiredContext::CheckRun {
                required: true,
                name: "test".to_string(),
                status: "COMPLETED".to_string(),
                conclusion: Some("FAILURE".to_string()),
                started_at: Some("2026-08-28T11:00:00Z".to_string()),
            },
            RequiredContext::CheckRun {
                required: true,
                name: "lint".to_string(),
                status: "IN_PROGRESS".to_string(),
                conclusion: None,
                started_at: Some("2026-08-28T10:00:00Z".to_string()),
            },
            RequiredContext::CheckRun {
                required: false,
                name: "optional".to_string(),
                status: "COMPLETED".to_string(),
                conclusion: Some("SUCCESS".to_string()),
                started_at: Some("2026-08-28T09:00:00Z".to_string()),
            },
            RequiredContext::StatusContext {
                required: true,
                context: "legacy".to_string(),
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
        assert_eq!(
            summary.contexts,
            HashSet::from(["test".into(), "lint".into(), "legacy".into()])
        );
    }

    #[test]
    fn required_contexts_combine_branch_protection_and_all_rule_pages() {
        let mut contexts = HashSet::new();
        for (page, expected_cursor) in [
            (
                serde_json::json!({
                    "refUpdateRule": { "requiredStatusCheckContexts": ["test", "legacy"] },
                    "rules": {
                        "pageInfo": { "hasNextPage": true, "endCursor": "NEXT" },
                        "nodes": [
                            { "parameters": null },
                            { "parameters": { "__typename": "PullRequestParameters" } },
                            { "parameters": {
                                "__typename": "RequiredStatusChecksParameters",
                                "requiredStatusChecks": [
                                    { "context": "test" }, { "context": "lint" }
                                ]
                            } }
                        ]
                    }
                }),
                Some("NEXT"),
            ),
            (
                serde_json::json!({
                    "refUpdateRule": { "requiredStatusCheckContexts": ["test", "legacy"] },
                    "rules": {
                        "pageInfo": { "hasNextPage": false, "endCursor": "LAST" },
                        "nodes": [{ "parameters": {
                            "__typename": "RequiredStatusChecksParameters",
                            "requiredStatusChecks": [{ "context": "security" }]
                        } }]
                    }
                }),
                None,
            ),
        ] {
            let rules: RequiredRef = serde_json::from_value(page).unwrap();
            assert_eq!(
                rules.add_to(&mut contexts).unwrap().as_deref(),
                expected_cursor
            );
        }
        assert_eq!(
            contexts,
            HashSet::from([
                "test".into(),
                "legacy".into(),
                "lint".into(),
                "security".into()
            ])
        );
    }

    #[test]
    fn default_branch_rules_resume_after_the_inline_page_when_used() {
        let branch: DefaultBranchRef = serde_json::from_value(serde_json::json!({
            "id": "default",
            "refUpdateRule": { "requiredStatusCheckContexts": ["classic"] },
            "rules": {
                "pageInfo": { "hasNextPage": true, "endCursor": "NEXT" },
                "nodes": [{
                    "parameters": {
                        "__typename": "RequiredStatusChecksParameters",
                        "requiredStatusChecks": [{ "context": "inline" }]
                    }
                }]
            }
        }))
        .unwrap();
        let mut cache = RequiredContextCache::default();
        seed_default_branch_ref(Some(branch), &mut cache).unwrap();
        assert_eq!(cache.pending["default"], "NEXT");

        let mut cursors = Vec::new();
        fetch_required_contexts(&["default".to_string()], &mut cache, |query, variables| {
            assert!(query.contains("rules(first: 100, after: $after0)"));
            assert_eq!(variables["id0"], "default");
            let after = variables["after0"].as_str().map(str::to_string);
            cursors.push(after.clone());
            assert_eq!(after.as_deref(), Some("NEXT"));
            Ok((
                RequiredRefsData {
                    refs: HashMap::from([(
                        "r0".to_string(),
                        serde_json::json!({
                            "refUpdateRule": null,
                            "rules": {
                                "pageInfo": { "hasNextPage": false, "endCursor": null },
                                "nodes": [{
                                    "parameters": {
                                        "__typename": "RequiredStatusChecksParameters",
                                        "requiredStatusChecks": [{ "context": "later" }]
                                    }
                                }]
                            }
                        }),
                    )]),
                },
                Vec::new(),
            ))
        })
        .unwrap();

        assert_eq!(cursors, [Some("NEXT".to_string())]);
        assert_eq!(
            cache.contexts["default"],
            HashSet::from(["classic".into(), "inline".into(), "later".into()])
        );
        assert!(!cache.pending.contains_key("default"));
    }

    fn required_rules_response() -> String {
        let required_rules = serde_json::json!({
            "refUpdateRule": { "requiredStatusCheckContexts": ["test", "lint"] },
            "rules": {
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": []
            }
        });
        let mut data = serde_json::json!({});
        for alias in 0..5 {
            let object = data.as_object_mut().unwrap();
            object.insert(format!("r{alias}"), required_rules.clone());
            object.insert(
                format!("c{alias}"),
                serde_json::json!({
                    "statusCheckRollup": {
                        "contexts": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": [{
                                "__typename": "CheckRun",
                                "required": true,
                                "name": "test",
                                "status": "COMPLETED",
                                "conclusion": "SUCCESS",
                                "startedAt": "2026-08-28T10:00:00Z"
                            }]
                        }
                    }
                }),
            );
        }
        serde_json::json!({ "data": data }).to_string()
    }

    fn required_rule_page(after: Option<&str>) -> serde_json::Value {
        let (has_next_page, end_cursor, context) = if after.is_none() {
            (true, Some("NEXT"), "test")
        } else {
            (false, Some("LAST"), "lint")
        };
        serde_json::json!({
            "refUpdateRule": if after.is_none() {
                serde_json::json!({ "requiredStatusCheckContexts": ["classic"] })
            } else {
                serde_json::Value::Null
            },
            "rules": {
                "pageInfo": { "hasNextPage": has_next_page, "endCursor": end_cursor },
                "nodes": [{
                    "parameters": {
                        "__typename": "RequiredStatusChecksParameters",
                        "requiredStatusChecks": [{ "context": context }]
                    }
                }]
            }
        })
    }

    #[test]
    fn shared_required_rule_errors_warn_every_consuming_section() {
        let error: GqlError = serde_json::from_value(serde_json::json!({
            "message": "required rules unavailable",
            "path": ["r0", "rules"]
        }))
        .unwrap();
        let mut cache = RequiredContextCache::default();
        let errors = fetch_required_contexts(&["ref".to_string()], &mut cache, |_, _| {
            Ok((
                RequiredRefsData {
                    refs: HashMap::from([(
                        "r0".to_string(),
                        serde_json::json!({
                            "refUpdateRule": { "requiredStatusCheckContexts": ["classic"] },
                            "rules": {
                                "pageInfo": { "hasNextPage": false, "endCursor": null },
                                "nodes": []
                            }
                        }),
                    )]),
                },
                vec![error.clone()],
            ))
        })
        .unwrap();
        assert_eq!(errors[0].message, "required rules unavailable");

        let (client, fake) = crate::github::fake::client([]);
        let (_, mine_errors) = fetch_required(
            &client,
            &[RequiredTarget {
                pull_request_number: 1,
                commit_id: None,
                base_ref_id: Some("ref".to_string()),
            }],
            false,
            &mut cache,
        )
        .unwrap();
        assert_eq!(mine_errors[0].message, "required rules unavailable");
        assert_eq!(fake.requests(), Vec::<String>::new());
    }

    #[test]
    fn required_rule_dispatch_counts_for_one_five_and_six_refs() {
        for (case, ref_count, expected_per_refresh) in
            [("one", 1, 2), ("five", 5, 2), ("six", 6, 4)]
        {
            for required_only in [false, true] {
                let (client, fake) = crate::github::fake::client([(
                    "POST /graphql".to_string(),
                    required_rules_response(),
                )]);
                let mut requests_per_refresh = Vec::new();
                for _ in 0..2 {
                    let request_count = fake.requests().len();
                    let mut cache = RequiredContextCache::default();
                    let targets: Vec<_> = (0..ref_count)
                        .map(|index| RequiredTarget {
                            pull_request_number: 100 + i64::from(index),
                            commit_id: Some(format!("commit-{index}")),
                            base_ref_id: Some(format!("ref-{index}")),
                        })
                        .collect();
                    let (summaries, errors) =
                        fetch_required(&client, &targets, required_only, &mut cache).unwrap();
                    assert!(errors.is_empty(), "{case}, {required_only}: {errors:?}");
                    for summary in summaries {
                        assert_eq!(summary.checks.pass, 1, "{case}, {required_only}");
                        assert_eq!(summary.missing, 1, "{case}, {required_only}");
                    }
                    requests_per_refresh.push(fake.requests().len() - request_count);
                }
                assert_eq!(
                    requests_per_refresh, [expected_per_refresh; 2],
                    "{case}, required_only={required_only}",
                );
            }
        }
    }

    #[test]
    fn required_rule_dispatch_counts_overlapping_and_disjoint_sections() {
        for required_only in [false, true] {
            let (client, fake) = crate::github::fake::client([(
                "POST /graphql".to_string(),
                required_rules_response(),
            )]);
            let mut requests_per_refresh = Vec::new();
            for _ in 0..2 {
                let request_count = fake.requests().len();
                let mut cache = RequiredContextCache::default();
                for (start, end) in [(0, 5), (3, 8)] {
                    let targets: Vec<_> = (start..end)
                        .map(|index| RequiredTarget {
                            pull_request_number: 100 + i64::try_from(index).unwrap(),
                            commit_id: Some(format!("commit-{index}")),
                            base_ref_id: Some(format!("ref-{index}")),
                        })
                        .collect();
                    let (summaries, errors) =
                        fetch_required(&client, &targets, required_only, &mut cache).unwrap();
                    assert!(errors.is_empty(), "{required_only}: {errors:?}");
                    assert_eq!(summaries.len(), end - start);
                    assert!(summaries.iter().all(|summary| summary.missing == 1));
                }
                requests_per_refresh.push(fake.requests().len() - request_count);
            }
            assert_eq!(
                requests_per_refresh, [4; 2],
                "required_only={required_only}"
            );
        }
    }

    #[test]
    fn required_rules_batch_pages_keep_each_ref_cursor() {
        let ids: Vec<_> = (0..6).map(|index| format!("ref-{index}")).collect();
        let mut cache = RequiredContextCache::default();
        let mut requests = Vec::new();
        fetch_required_contexts(&ids, &mut cache, |query, variables| {
            let count = variables.as_object().unwrap().len() / 2;
            assert!((1..=REQUIRED_CONTEXTS_BATCH_SIZE).contains(&count));
            let mut request = Vec::new();
            let mut refs = HashMap::new();
            for alias in 0..count {
                assert!(query.contains(&format!("r{alias}: node(id: $id{alias})")));
                assert!(query.contains(&format!("rules(first: 100, after: $after{alias})")));
                let id = variables[format!("id{alias}")].as_str().unwrap();
                let after = variables[format!("after{alias}")]
                    .as_str()
                    .map(str::to_string);
                let (has_next, end_cursor, context) = match (id, after.as_deref()) {
                    ("ref-0", None) => (true, Some("A"), "ref-0-first"),
                    ("ref-0", Some("A")) => (false, None, "ref-0-second"),
                    ("ref-1", None) => (true, Some("B"), "ref-1-first"),
                    ("ref-1", Some("B")) => (false, None, "ref-1-second"),
                    _ => (false, None, "single-page"),
                };
                let rule = serde_json::json!({
                    "refUpdateRule": if after.is_none() {
                        serde_json::json!({ "requiredStatusCheckContexts": [format!("classic-{id}")] })
                    } else {
                        serde_json::Value::Null
                    },
                    "rules": {
                        "pageInfo": { "hasNextPage": has_next, "endCursor": end_cursor },
                        "nodes": [{
                            "parameters": {
                                "__typename": "RequiredStatusChecksParameters",
                                "requiredStatusChecks": [{ "context": context }]
                            }
                        }]
                    }
                });
                request.push((id.to_string(), after));
                refs.insert(format!("r{alias}"), rule);
            }
            requests.push(request);
            Ok((RequiredRefsData { refs }, Vec::new()))
        })
        .unwrap();

        assert_eq!(
            requests,
            [
                (0..5)
                    .map(|index| (format!("ref-{index}"), None))
                    .collect::<Vec<_>>(),
                vec![("ref-5".to_string(), None)],
                vec![
                    ("ref-0".to_string(), Some("A".to_string())),
                    ("ref-1".to_string(), Some("B".to_string())),
                ],
            ]
        );
        for id in ids {
            assert_eq!(
                cache.contexts[&id].len(),
                if id == "ref-0" || id == "ref-1" { 3 } else { 2 },
                "{id}"
            );
        }
    }

    #[test]
    fn required_rules_missing_alias_is_an_error_and_caches_nothing() {
        let ids = ["ref-0".to_string(), "ref-1".to_string()];
        for (refs, expected_error) in [
            (
                HashMap::from([("r0".to_string(), required_rule_page(None))]),
                "required-rule response omitted r1",
            ),
            (
                HashMap::from([
                    ("r0".to_string(), serde_json::Value::Null),
                    ("r1".to_string(), required_rule_page(None)),
                ]),
                "required-check base branch ref-0 was unavailable",
            ),
        ] {
            let mut cache = RequiredContextCache::default();
            let error = fetch_required_contexts(&ids, &mut cache, |_, _| {
                Ok((RequiredRefsData { refs: refs.clone() }, Vec::new()))
            })
            .unwrap_err();
            assert!(error.to_string().contains(expected_error), "{error:#}");
            assert!(cache.contexts.is_empty());
        }
    }

    #[test]
    fn required_rule_cache_only_stores_complete_paginated_results() {
        let mut cache = RequiredContextCache::default();
        let mut failed_cursors = Vec::new();
        let ids = ["ref".to_string()];
        let error = fetch_required_contexts(&ids, &mut cache, |query, variables| {
            assert!(query.contains("r0: node(id: $id0)"));
            let after = variables["after0"].as_str().map(str::to_string);
            failed_cursors.push(after.clone());
            if after.is_some() {
                anyhow::bail!("second rules page failed");
            }
            Ok((
                RequiredRefsData {
                    refs: HashMap::from([("r0".to_string(), required_rule_page(after.as_deref()))]),
                },
                Vec::new(),
            ))
        });
        assert!(error.is_err());
        assert_eq!(failed_cursors, [None, Some("NEXT".to_string())]);
        assert!(!cache.contexts.contains_key("ref"));

        let mut retried_cursors = Vec::new();
        fetch_required_contexts(&ids, &mut cache, |query, variables| {
            assert!(query.contains("r0: node(id: $id0)"));
            let after = variables["after0"].as_str().map(str::to_string);
            retried_cursors.push(after.clone());
            Ok((
                RequiredRefsData {
                    refs: HashMap::from([("r0".to_string(), required_rule_page(after.as_deref()))]),
                },
                Vec::new(),
            ))
        })
        .unwrap();
        assert_eq!(retried_cursors, [None, Some("NEXT".to_string())]);
        assert_eq!(
            cache.contexts["ref"],
            HashSet::from(["classic".into(), "test".into(), "lint".into()])
        );
    }

    #[test]
    fn failed_rule_response_errors_do_not_taint_a_successful_retry() {
        let error: GqlError = serde_json::from_value(serde_json::json!({
            "message": "required rules unavailable",
            "path": ["r0", "rules"]
        }))
        .unwrap();
        let mut cache = RequiredContextCache::default();
        let ids = ["ref".to_string()];
        let failed = fetch_required_contexts(&ids, &mut cache, |_, _| {
            Ok((
                RequiredRefsData {
                    refs: HashMap::from([("r0".to_string(), serde_json::Value::Null)]),
                },
                vec![error.clone()],
            ))
        });
        assert!(failed.is_err());
        assert!(!cache.errors.contains_key("ref"));

        fetch_required_contexts(&ids, &mut cache, |_, variables| {
            let after = variables["after0"].as_str();
            Ok((
                RequiredRefsData {
                    refs: HashMap::from([("r0".to_string(), required_rule_page(after))]),
                },
                Vec::new(),
            ))
        })
        .unwrap();

        let (client, fake) = crate::github::fake::client([]);
        let (_, errors) = fetch_required(
            &client,
            &[RequiredTarget {
                pull_request_number: 1,
                commit_id: None,
                base_ref_id: Some("ref".to_string()),
            }],
            false,
            &mut cache,
        )
        .unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(fake.requests(), Vec::<String>::new());
    }

    #[test]
    fn unprotected_branches_have_no_expected_checks() {
        for update_rule in [
            serde_json::Value::Null,
            serde_json::json!({ "requiredStatusCheckContexts": null }),
            serde_json::json!({ "requiredStatusCheckContexts": [] }),
        ] {
            let rules: RequiredRef = serde_json::from_value(serde_json::json!({
                "refUpdateRule": update_rule,
                "rules": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": []
                }
            }))
            .unwrap();
            let mut contexts = HashSet::new();
            assert_eq!(rules.add_to(&mut contexts).unwrap(), None);
            assert!(contexts.is_empty(), "{contexts:?}");
        }
    }

    #[test]
    fn graphql_pages_need_a_cursor_to_continue() {
        let page = PageInfo {
            has_next_page: true,
            end_cursor: None,
        };
        assert_eq!(
            page.next_cursor().unwrap_err().to_string(),
            "GraphQL page had no end cursor"
        );
    }

    fn pr_without_checks() -> PrNode {
        let data: MineData =
            crate::github::parse_graphql(include_bytes!("../tests/fixtures/mine_no_rollup.json"))
                .unwrap();
        data.search.nodes.into_iter().next().unwrap()
    }

    #[test]
    fn missing_required_checks_stay_in_run_until_they_report() {
        let expected = HashSet::from(["test".into(), "legacy".into()]);
        for required_only in [false, true] {
            for (state, fail, running, pass) in [
                (None, 0, 2, 0),
                (Some("QUEUED"), 0, 2, 0),
                (Some("IN_PROGRESS"), 0, 2, 0),
                (Some("SUCCESS"), 0, 1, 1),
                (Some("FAILURE"), 1, 1, 0),
                (Some("SKIPPED"), 0, 1, 1),
            ] {
                let mut node = pr_without_checks();
                let mut counts = vec![StateCount {
                    state: "SUCCESS".into(),
                    count: 3,
                }];
                let mut summary = RequiredSummary::default();
                // An optional check with the same name is not the required job.
                RequiredContext::CheckRun {
                    required: false,
                    name: "legacy".into(),
                    status: "COMPLETED".into(),
                    conclusion: Some("SUCCESS".into()),
                    started_at: None,
                }
                .add_to(&mut summary);
                if let Some(state) = state {
                    counts.push(StateCount {
                        state: state.into(),
                        count: 1,
                    });
                    RequiredContext::CheckRun {
                        required: true,
                        name: "test".into(),
                        status: state.into(),
                        conclusion: None,
                        started_at: None,
                    }
                    .add_to(&mut summary);
                }
                node.commits.nodes[0].commit.status_check_rollup = Some(Rollup {
                    contexts: RollupCounts {
                        check_runs: counts,
                        status_contexts: vec![],
                    },
                });
                summary.include_missing(&expected);
                node.apply_required(summary, required_only);
                let rows = crate::prs::build_rows(vec![node], OpenSort::Updated);
                assert_eq!(
                    rows[0].checks,
                    Checks {
                        fail,
                        running,
                        pass: pass + if required_only { 0 } else { 3 }
                    },
                    "required_only={required_only}, state={state:?}"
                );
                assert_eq!(
                    rows[0].status,
                    Some(if fail > 0 {
                        status::Status::Fail
                    } else {
                        status::Status::Pending
                    })
                );
                for ascii in [false, true] {
                    let table = crate::prs::to_table(&rows, ascii, &HashSet::new(), false);
                    assert_eq!(table.rows[0][6].text, running.to_string());
                }
            }
        }
    }

    #[test]
    fn missing_required_checks_count_even_without_a_rollup() {
        for required_only in [false, true] {
            let mut node = pr_without_checks();
            let mut summary = RequiredSummary::default();
            summary.include_missing(&HashSet::from(["test".into(), "lint".into()]));
            node.apply_required(summary, required_only);
            assert_eq!(
                node.checks(),
                Checks {
                    fail: 0,
                    running: 2,
                    pass: 0
                }
            );
        }
    }

    #[test]
    fn reported_legacy_statuses_are_not_counted_as_missing() {
        for state in ["EXPECTED", "PENDING", "SUCCESS", "FAILURE"] {
            let mut node = pr_without_checks();
            let mut summary = RequiredSummary::default();
            RequiredContext::StatusContext {
                required: true,
                context: "legacy".into(),
                state: state.into(),
            }
            .add_to(&mut summary);
            let checks = summary.checks;
            summary.include_missing(&HashSet::from(["legacy".into()]));
            node.apply_required(summary, true);
            assert_eq!(node.checks(), checks, "{state}");
            assert_eq!(node.missing_required_checks, 0, "{state}");
        }
    }

    fn queue_entry(head_commit: serde_json::Value) -> QueueEntryNode {
        serde_json::from_value(serde_json::json!({
            "position": 1,
            "headCommit": head_commit,
            "pullRequest": {
                "number": 42,
                "title": "Queued PR",
                "url": "https://github.com/owner/repo/pull/42",
                "baseRef": { "id": "BASE_REF" }
            }
        }))
        .unwrap()
    }

    #[test]
    fn queue_targets_the_speculative_commit_and_pr_base() {
        assert!(QUEUE_QUERY.contains("baseRef { id }"), "{QUEUE_QUERY}");
        for id in [Some("SPECULATIVE_COMMIT"), None] {
            let node = queue_entry(id.map_or(
                serde_json::Value::Null,
                |id| serde_json::json!({ "id": id, "statusCheckRollup": null }),
            ));
            let target = node.required_target();
            assert_eq!(target.commit_id.as_deref(), id);
            assert_eq!(target.base_ref_id.as_deref(), Some("BASE_REF"));
            assert_eq!(target.pull_request_number, 42);
        }
    }

    #[test]
    fn queue_missing_required_checks_count_before_building() {
        for required_only in [false, true] {
            for head in [
                serde_json::Value::Null,
                serde_json::json!({ "id": "SPECULATIVE", "statusCheckRollup": null }),
            ] {
                let mut node = queue_entry(head);
                let mut summary = RequiredSummary::default();
                summary.include_missing(&HashSet::from(["test".into(), "lint".into()]));
                node.apply_required(summary, required_only);
                let rows = crate::queue::build_rows(vec![node], "me");
                assert_eq!(
                    rows[0].checks,
                    Checks {
                        fail: 0,
                        running: 2,
                        pass: 0
                    },
                    "required_only={required_only}"
                );
                assert_eq!(rows[0].missing_required_checks, 2);
                assert_eq!(rows[0].build_started_at, None);
                for ascii in [false, true] {
                    let table = crate::queue::to_table(&rows, ascii, false);
                    assert_eq!(table.rows[0][8].text, "2");
                    assert_eq!(table.rows[0][6].text, "\u{2014}");
                }
            }
        }
    }

    #[test]
    fn queue_reported_checks_are_not_also_missing() {
        for required_only in [false, true] {
            for (state, fail, running, pass, started) in [
                ("QUEUED", 0, 2, 1, None),
                ("IN_PROGRESS", 0, 2, 1, Some("2026-06-19T12:00:00Z")),
                ("SUCCESS", 0, 1, 2, Some("2026-06-19T12:00:00Z")),
                ("FAILURE", 1, 1, 1, Some("2026-06-19T12:00:00Z")),
            ] {
                let mut node = queue_entry(serde_json::json!({
                    "id": "SPECULATIVE",
                    "statusCheckRollup": { "contexts": {
                        "checkRunCountsByState": [
                            { "state": "SUCCESS", "count": 1 },
                            { "state": state, "count": 1 }
                        ],
                        "statusContextCountsByState": [{ "state": "SUCCESS", "count": 1 }],
                        "nodes": [
                            { "startedAt": "2026-06-19T11:00:00Z" },
                            { "startedAt": started }
                        ]
                    } }
                }));
                let mut summary = RequiredSummary::default();
                for context in [
                    RequiredContext::CheckRun {
                        required: false,
                        name: "missing".into(),
                        status: "COMPLETED".into(),
                        conclusion: Some("SUCCESS".into()),
                        started_at: Some("2026-06-19T11:00:00Z".into()),
                    },
                    RequiredContext::CheckRun {
                        required: true,
                        name: "test".into(),
                        status: state.into(),
                        conclusion: None,
                        started_at: started.map(str::to_string),
                    },
                    RequiredContext::StatusContext {
                        required: true,
                        context: "legacy".into(),
                        state: "SUCCESS".into(),
                    },
                ] {
                    context.add_to(&mut summary);
                }
                summary.include_missing(&HashSet::from([
                    "test".into(),
                    "legacy".into(),
                    "missing".into(),
                ]));
                node.apply_required(summary, required_only);
                let rows = crate::queue::build_rows(vec![node], "me");
                assert_eq!(
                    rows[0].checks,
                    Checks {
                        fail,
                        running,
                        pass: pass + u64::from(!required_only)
                    },
                    "required_only={required_only}, state={state}"
                );
                assert_eq!(rows[0].missing_required_checks, 1);
                assert_eq!(
                    rows[0].build_started_at.as_deref(),
                    if required_only {
                        started
                    } else {
                        Some("2026-06-19T11:00:00Z")
                    },
                    "required_only={required_only}, state={state}"
                );
            }
        }
    }

    #[test]
    fn required_checks_on_later_pages_are_not_missing() {
        let first_page = (0..100)
            .map(|n| {
                serde_json::json!({
                    "__typename": "CheckRun",
                    "required": false,
                    "name": format!("optional-{n}"),
                    "status": "COMPLETED",
                    "conclusion": "SUCCESS"
                })
            })
            .collect::<Vec<_>>();
        let mut summary = RequiredSummary::default();
        for (nodes, cursor) in [
            (first_page, Some("NEXT")),
            (
                vec![serde_json::json!({
                    "__typename": "CheckRun",
                    "required": true,
                    "name": "test",
                    "status": "COMPLETED",
                    "conclusion": "SUCCESS"
                })],
                None,
            ),
        ] {
            let commit = serde_json::from_value(serde_json::json!({
                "statusCheckRollup": { "contexts": {
                    "pageInfo": { "hasNextPage": cursor.is_some(), "endCursor": cursor },
                    "nodes": nodes
                } }
            }))
            .unwrap();
            assert_eq!(summary.add_page(commit).unwrap().as_deref(), cursor);
        }
        summary.include_missing(&HashSet::from(["test".into(), "missing".into()]));
        assert_eq!(summary.missing, 1);
        assert_eq!(
            summary.checks,
            Checks {
                fail: 0,
                running: 0,
                pass: 1
            }
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

    fn required_commit_page(target: usize, page: usize, next: Option<String>) -> RequiredCommit {
        let nodes: Vec<_> = (0..=target)
            .map(|check| {
                serde_json::json!({
                    "__typename": "CheckRun",
                    "required": true,
                    "name": format!("check-{target}-{page}-{check}"),
                    "status": if page == 2 { "QUEUED" } else { "COMPLETED" },
                    "conclusion": match page {
                        0 => Some("SUCCESS"),
                        1 => Some("FAILURE"),
                        _ => None,
                    }
                })
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "statusCheckRollup": { "contexts": {
                "pageInfo": {
                    "hasNextPage": next.is_some(),
                    "endCursor": next
                },
                "nodes": nodes
            } }
        }))
        .unwrap()
    }

    #[test]
    fn required_pages_batch_commits_and_preserve_each_targets_cursor_and_counts() {
        let pages = [1usize, 2, 0, 3, 1, 2, 0, 3];
        let pending = pages
            .iter()
            .enumerate()
            .filter(|(_, pages)| **pages > 0)
            .map(|(target, _)| PendingRequired {
                target,
                pull_request_number: 100 + i64::try_from(target).unwrap(),
                commit_id: format!("commit-{target}"),
                after: None,
            })
            .collect();
        let mut summaries: Vec<_> = pages.iter().map(|_| RequiredSummary::default()).collect();
        let mut received = [0usize; 8];
        let mut requests = Vec::new();

        fetch_required_pages(pending, &mut summaries, |query, variables| {
            let count = query.matches(": node(id: $commit").count();
            assert!((1..=5).contains(&count), "commit count: {count}");
            assert_eq!(variables.as_object().unwrap().len(), count * 3);
            let mut response = HashMap::new();
            let mut request = Vec::new();
            for alias in 0..count {
                let commit = variables[format!("commit{alias}")].as_str().unwrap();
                let target: usize = commit.strip_prefix("commit-").unwrap().parse().unwrap();
                let page = received[target];
                assert!(page < pages[target], "extra page for target {target}");
                assert_eq!(
                    variables[format!("pull{alias}")],
                    100 + i64::try_from(target).unwrap(),
                    "PR number for target {target}"
                );
                let after = (page > 0).then(|| format!("cursor-{target}-{page}"));
                assert_eq!(
                    variables[format!("after{alias}")],
                    serde_json::json!(after),
                    "cursor for target {target}, page {page}"
                );
                assert!(query.contains(&format!("c{alias}: node(id: $commit{alias})")));
                assert!(query.contains(&format!("contexts(first: 100, after: $after{alias})")));
                assert!(query.contains(&format!("isRequired(pullRequestNumber: $pull{alias})")));
                request.push((target, page));
                received[target] += 1;
                let next = (received[target] < pages[target])
                    .then(|| format!("cursor-{target}-{}", page + 1));
                response.insert(
                    format!("c{alias}"),
                    Some(required_commit_page(target, page, next)),
                );
            }
            requests.push(request);
            Ok(response)
        })
        .unwrap();

        assert_eq!(
            requests,
            [
                vec![(0, 0), (1, 0), (3, 0), (4, 0), (5, 0)],
                vec![(7, 0)],
                vec![(1, 1), (3, 1), (5, 1), (7, 1)],
                vec![(3, 2), (7, 2)],
            ]
        );
        assert_eq!(received, pages);
        for (target, summary) in summaries.iter().enumerate() {
            let checks = (target + 1) as u64;
            assert_eq!(
                summary.checks,
                Checks {
                    pass: if pages[target] > 0 { checks } else { 0 },
                    fail: if pages[target] > 1 { checks } else { 0 },
                    running: if pages[target] > 2 { checks } else { 0 },
                },
                "counts for original target {target}"
            );
            let contexts: HashSet<_> = (0..pages[target])
                .flat_map(|page| {
                    (0..=target).map(move |check| format!("check-{target}-{page}-{check}"))
                })
                .collect();
            assert_eq!(
                summary.contexts, contexts,
                "contexts for original target {target}"
            );
        }
    }
}

//! Typed serde models for the three `gh api graphql` queries, plus the fetch
//! helpers that run them. Queries are sent verbatim (the merged query's page
//! size is the only thing we interpolate, so `--merged-limit` is honored).

use crate::gh::{self, Repo};
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
    #[serde(rename = "pullRequest")]
    pub pull_request: QueuePr,
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
pub fn fetch_queue(repo: &Repo) -> Result<Vec<QueueEntryNode>> {
    let data: QueueData = gh::graphql(
        &[("owner", repo.owner.as_str()), ("name", repo.name.as_str())],
        QUEUE_QUERY,
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
        number title url state mergeable mergeStateStatus isDraft
        mergeQueueEntry { position state }
        commits(last: 1) { nodes { commit { checkSuites(first: 50) { nodes { conclusion } } } } }
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
    pub nodes: Vec<CheckSuite>,
}

#[derive(Debug, Deserialize)]
pub struct CheckSuite {
    pub conclusion: Option<String>,
}

pub fn mine_search(repo: &Repo, me: &str) -> String {
    format!(
        "repo:{}/{} is:pr is:open author:{} archived:false",
        repo.owner, repo.name, me
    )
}

pub fn fetch_my_prs(repo: &Repo, me: &str) -> Result<Vec<PrNode>> {
    let q = mine_search(repo, me);
    let data: MineData = gh::graphql(&[("q", q.as_str())], MINE_QUERY)?;
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
    #[serde(rename = "mergedAt")]
    pub merged_at: Option<String>,
    #[serde(rename = "baseRefName")]
    pub base_ref_name: Option<String>,
}

/// The recently-merged query; `first` is the page size (clamped 1..=100).
pub fn merged_query(limit: usize) -> String {
    let first = limit.clamp(1, 100);
    format!(
        r#"query($q: String!) {{
  search(type: ISSUE, first: {first}, query: $q) {{
    nodes {{
      ... on PullRequest {{
        number title url mergedAt baseRefName
      }}
    }}
  }}
}}"#
    )
}

pub fn merged_search(repo: &Repo, me: &str, since: &str) -> String {
    format!(
        "repo:{}/{} is:pr is:merged author:{} merged:>={} sort:updated-desc",
        repo.owner, repo.name, me, since
    )
}

pub fn fetch_merged(repo: &Repo, me: &str, since: &str, limit: usize) -> Result<Vec<MergedNode>> {
    let q = merged_search(repo, me, since);
    let data: MergedData = gh::graphql(&[("q", q.as_str())], &merged_query(limit))?;
    Ok(data.search.nodes)
}

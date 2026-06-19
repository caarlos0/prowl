//! Thin wrapper around the `gh` CLI.
//!
//! We never handle tokens or speak HTTP ourselves: `gh` already knows how to
//! authenticate, and shelling out matches the scripts this tool replaces.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::process::Command;

/// A `owner/name` repository slug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub owner: String,
    pub name: String,
}

impl Repo {
    /// Parse an `owner/name` slug.
    pub fn parse(slug: &str) -> Result<Repo> {
        let (owner, name) = slug
            .trim()
            .split_once('/')
            .ok_or_else(|| anyhow!("invalid repo `{slug}`, expected owner/name"))?;
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            bail!("invalid repo `{slug}`, expected owner/name");
        }
        Ok(Repo {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }

    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// Run `gh` with `args`, returning stdout on success.
///
/// On a non-zero exit we surface stderr so the caller can show a dim error
/// line and keep retrying.
pub fn run(args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("gh")
        .args(args)
        .output()
        .context("failed to run `gh` (is the GitHub CLI installed and on PATH?)")?;
    if !out.status.success() {
        // A short label (e.g. "api graphql") rather than the full args, which
        // for GraphQL would dump the entire query.
        let label = args.iter().take(2).copied().collect::<Vec<_>>().join(" ");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = stderr.trim();
        if detail.is_empty() {
            bail!("`gh {label}` failed ({})", out.status);
        }
        bail!("`gh {label}` failed: {detail}");
    }
    Ok(out.stdout)
}

/// Run `gh` and return trimmed stdout as a `String`.
fn run_str(args: &[&str]) -> Result<String> {
    let bytes = run(args)?;
    Ok(String::from_utf8(bytes)
        .context("`gh` produced non-UTF-8 output")?
        .trim()
        .to_string())
}

/// The authenticated user's login.
pub fn me() -> Result<String> {
    let login = run_str(&["api", "user", "--jq", ".login"])?;
    if login.is_empty() {
        bail!("could not determine the current user from `gh api user`");
    }
    Ok(login)
}

/// The repository's default branch (e.g. `main`).
pub fn default_branch(repo: &Repo) -> Result<String> {
    let branch = run_str(&[
        "api",
        &format!("repos/{}/{}", repo.owner, repo.name),
        "--jq",
        ".default_branch",
    ])?;
    if branch.is_empty() {
        bail!("could not determine the default branch for {}", repo.slug());
    }
    Ok(branch)
}

/// Detect the current repository.
///
/// `gh repo view` misbehaves inside worktrees (cli/cli#1837), so when it fails
/// we fall back to the configured default base repo.
pub fn detect_repo() -> Result<Repo> {
    if let Ok(slug) = run_str(&[
        "repo",
        "view",
        "--json",
        "nameWithOwner",
        "--jq",
        ".nameWithOwner",
    ]) && !slug.is_empty()
    {
        return Repo::parse(&slug);
    }
    let slug = run_str(&["repo", "set-default", "--view"])
        .context("could not detect the repository; pass --repo owner/name")?;
    Repo::parse(&slug)
}

/// GraphQL response envelope: `{"data": ...}`.
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    data: T,
}

/// Run a GraphQL query through `gh api graphql`, parsing the full
/// `{"data": ...}` envelope into `T`.
///
/// Variables are passed as `-f key=value` (always strings). We deliberately do
/// not pass `-q`/`--jq`; the typed structs do the extraction.
pub fn graphql<T: DeserializeOwned>(vars: &[(&str, &str)], query: &str) -> Result<T> {
    let mut args: Vec<String> = vec!["api".into(), "graphql".into()];
    for (k, v) in vars {
        args.push("-f".into());
        args.push(format!("{k}={v}"));
    }
    args.push("-f".into());
    args.push(format!("query={query}"));

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let bytes = run(&arg_refs)?;
    parse_graphql(&bytes)
}

/// Parse a GraphQL `{"data": ...}` envelope from raw bytes.
///
/// Split out so tests can exercise it against captured fixtures offline.
pub fn parse_graphql<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let env: Envelope<T> =
        serde_json::from_slice(bytes).context("failed to parse `gh api graphql` JSON")?;
    Ok(env.data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_slug() {
        let r = Repo::parse("goreleaser/goreleaser").unwrap();
        assert_eq!(r.owner, "goreleaser");
        assert_eq!(r.name, "goreleaser");
        assert_eq!(r.slug(), "goreleaser/goreleaser");
    }

    #[test]
    fn rejects_bad_slugs() {
        for bad in ["", "noslash", "a/b/c", "/name", "owner/"] {
            assert!(Repo::parse(bad).is_err(), "expected `{bad}` to be rejected");
        }
    }
}

//! Native GitHub API transport over HTTP (replaces shelling out to `gh`).

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::process::Command;
use std::time::Duration;

const API: &str = "https://api.github.com";
const USER_AGENT: &str = concat!("prowl/", env!("CARGO_PKG_VERSION"));

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

/// Auto-detect the repository from the local git `origin` remote.
pub fn detect_repo() -> Result<Repo> {
    let out = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .context("could not run `git` to detect the repository; pass --repo owner/name")?;
    if !out.status.success() {
        bail!("no git `origin` remote found; pass --repo owner/name");
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    parse_remote(&url)
        .ok_or_else(|| anyhow!("could not parse owner/name from `{url}`; pass --repo owner/name"))
}

/// Extract `owner/name` from a github.com remote URL (https or ssh forms).
///
/// The host is compared exactly so look-alikes such as `github.com.evil.tld`
/// or `notgithub.com` are rejected.
fn parse_remote(url: &str) -> Option<Repo> {
    let s = url.trim().trim_end_matches(".git");
    let s = s.split_once("://").map_or(s, |(_, rest)| rest); // drop any scheme
    let s = s.rsplit_once('@').map_or(s, |(_, rest)| rest); // drop any user@
    let rest = s.strip_prefix("github.com")?; // exact host prefix
    let rest = rest.strip_prefix([':', '/'])?; // host must end at a separator
    let (owner, name) = rest.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(Repo {
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

/// GraphQL response envelope: `{"data": ..., "errors": [...]}`.
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Option<Vec<GqlError>>,
}

#[derive(Debug, Deserialize)]
struct GqlError {
    message: String,
}

/// Parse a GraphQL `{"data": ...}` envelope. GitHub returns partial `data`
/// alongside an `errors` array when some nested field is inaccessible (those
/// fields deserialize as `None`), so prefer the data when it parses.
///
/// Parse into `Value` first, never straight into `T`: a partial response can
/// null a required (non-`Option`) field while `data` is present, which would
/// fail a direct `from_slice::<Envelope<T>>` and mask the real GitHub error
/// with a generic parse error. When typing the data fails, surface the GraphQL
/// `errors` message if there is one.
/// Split out so tests can exercise it against captured fixtures offline.
pub fn parse_graphql<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let env: Envelope<serde_json::Value> =
        serde_json::from_slice(bytes).context("failed to parse GraphQL JSON")?;
    let first_error = || env.errors.as_ref().and_then(|e| e.first());
    if let Some(data) = env.data {
        return match serde_json::from_value(data) {
            Ok(typed) => Ok(typed),
            Err(err) => match first_error() {
                Some(gql) => bail!("GraphQL error: {}", gql.message),
                None => Err(err).context("failed to parse GraphQL data"),
            },
        };
    }
    if let Some(gql) = first_error() {
        bail!("GraphQL error: {}", gql.message);
    }
    bail!("GraphQL response had no data")
}

/// An authenticated GitHub API client.
pub struct Client {
    agent: ureq::Agent,
    token: String,
}

impl Client {
    pub fn new(token: String) -> Client {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .user_agent(USER_AGENT)
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();
        Client { agent, token }
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// Run a GraphQL query, returning the typed `data` payload.
    pub fn graphql<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<T> {
        let body = serde_json::json!({ "query": query, "variables": variables });
        let mut resp = self
            .agent
            .post(format!("{API}/graphql"))
            .header("Authorization", self.bearer())
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send_json(&body)
            .map_err(api_err)?;
        let bytes = resp
            .body_mut()
            .read_to_vec()
            .context("reading GraphQL response")?;
        parse_graphql(&bytes)
    }

    /// GET a REST path (relative to api.github.com), returning typed JSON.
    pub fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let mut resp = self
            .agent
            .get(format!("{API}/{path}"))
            .header("Authorization", self.bearer())
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .call()
            .map_err(api_err)?;
        resp.body_mut()
            .read_json()
            .with_context(|| format!("parsing response from /{path}"))
    }

    /// The authenticated user's login.
    pub fn me(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct User {
            login: String,
        }
        let user: User = self.get("user")?;
        Ok(user.login)
    }

    /// The repository's default branch (e.g. `main`).
    pub fn default_branch(&self, repo: &Repo) -> Result<String> {
        #[derive(Deserialize)]
        struct R {
            default_branch: String,
        }
        let r: R = self.get(&format!("repos/{}/{}", repo.owner, repo.name))?;
        Ok(r.default_branch)
    }
}

/// Turn a ureq error into a concise message; flags auth failures.
fn api_err(e: ureq::Error) -> anyhow::Error {
    if let ureq::Error::StatusCode(401) = e {
        return anyhow!("GitHub rejected the token (401); run `prowl --login`");
    }
    anyhow!("GitHub request failed: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remote_forms() {
        for url in [
            "https://github.com/goreleaser/goreleaser.git",
            "https://github.com/goreleaser/goreleaser",
            "git@github.com:goreleaser/goreleaser.git",
            "ssh://git@github.com/goreleaser/goreleaser.git",
        ] {
            let r = parse_remote(url).expect(url);
            assert_eq!(r.owner, "goreleaser");
            assert_eq!(r.name, "goreleaser");
        }
        for url in [
            "https://gitlab.com/a/b.git",
            "https://github.com.evil.tld/a/b.git",
            "https://notgithub.com/a/b.git",
            "git@github.com.evil.tld:a/b.git",
            "ssh://git@notgithub.com/a/b.git",
        ] {
            assert!(parse_remote(url).is_none(), "{url}");
        }
    }

    #[test]
    fn parses_slug() {
        assert!(Repo::parse("a/b").is_ok());
        for bad in ["", "noslash", "a/b/c", "/b", "a/"] {
            assert!(Repo::parse(bad).is_err(), "{bad}");
        }
    }
}

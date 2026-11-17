//! Native GitHub API transport over HTTP (replaces shelling out to `gh`).

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, TimeDelta, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use ureq::http::{HeaderMap, Response};

const API: &str = "https://api.github.com";
const USER_AGENT: &str = concat!("prowl/", env!("CARGO_PKG_VERSION"));
const MAX_CACHED_GETS: usize = 32;

#[derive(Clone)]
struct CachedGet {
    etag: String,
    body: Arc<serde_json::Value>,
}

#[derive(Default)]
struct GetCache {
    entries: HashMap<String, CachedGet>,
    insertion_order: VecDeque<String>,
}

impl GetCache {
    fn insert(&mut self, path: String, entry: CachedGet) {
        if let Some(cached) = self.entries.get_mut(&path) {
            *cached = entry;
            self.insertion_order.retain(|cached| cached != &path);
            self.insertion_order.push_back(path);
            return;
        }
        while self.entries.len() >= MAX_CACHED_GETS {
            if let Some(oldest) = self.insertion_order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
        self.insertion_order.push_back(path.clone());
        self.entries.insert(path, entry);
    }

    fn remove(&mut self, path: &str) {
        if self.entries.remove(path).is_some() {
            self.insertion_order.retain(|cached| cached != path);
        }
    }
}

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

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GqlError {
    pub(crate) message: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    pub(crate) path: Option<Vec<serde_json::Value>>,
}

/// Parse the `data` value from a GraphQL `{"data": ...}` envelope. GitHub
/// returns partial `data` alongside an `errors` array when some nested field is
/// inaccessible; use `Client::graphql_with_errors` when callers need to retain
/// those errors after the data parses.
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
    env.into_data()
}

impl Envelope<serde_json::Value> {
    fn into_data<T: DeserializeOwned>(self) -> Result<T> {
        self.into_data_with_errors().map(|(data, _)| data)
    }

    fn into_data_with_errors<T: DeserializeOwned>(self) -> Result<(T, Vec<GqlError>)> {
        let errors = self.errors.unwrap_or_default();
        let first_error = errors.first();
        if let Some(data) = self.data {
            let typed = match serde_json::from_value(data) {
                Ok(typed) => typed,
                Err(err) => match first_error {
                    Some(gql) => bail!("GraphQL error: {}", gql.message),
                    None => return Err(err).context("failed to parse GraphQL data"),
                },
            };
            return Ok((typed, errors));
        }
        if let Some(gql) = first_error {
            bail!("GraphQL error: {}", gql.message);
        }
        bail!("GraphQL response had no data")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Resource {
    Core,
    Graphql,
    Secondary,
}

impl Resource {
    fn error(self, reset: DateTime<Utc>) -> anyhow::Error {
        anyhow::Error::new(RateLimited {
            resource: self,
            reset,
        })
    }
}

/// A local refusal because `resource` is cooling down. `Client::me` downcasts
/// it to tell a core limit from the others.
#[derive(Debug)]
struct RateLimited {
    resource: Resource,
    reset: DateTime<Utc>,
}

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self.resource {
            Resource::Core => "core",
            Resource::Graphql => "graphql",
            Resource::Secondary => "secondary",
        };
        write!(
            f,
            "GitHub {name} rate limit; retry after {}",
            self.reset.format("%Y-%m-%dT%H:%M:%S%.fZ")
        )
    }
}

impl std::error::Error for RateLimited {}

fn secondary_limit(message: &str) -> bool {
    message
        .to_ascii_lowercase()
        .contains("secondary rate limit")
}

/// An authenticated GitHub API client.
#[derive(Clone)]
pub struct Client {
    agent: ureq::Agent,
    token: String,
    cooldowns: Arc<Mutex<HashMap<Resource, DateTime<Utc>>>>,
    get_cache: Arc<Mutex<GetCache>>,
    #[cfg(test)]
    fake: Option<fake::Fake>,
}

impl Client {
    pub fn new(token: String) -> Client {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .user_agent(USER_AGENT)
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            .build()
            .into();
        Client {
            agent,
            token,
            cooldowns: Arc::default(),
            get_cache: Arc::default(),
            #[cfg(test)]
            fake: None,
        }
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
        self.graphql_with_errors(query, variables)
            .map(|(data, _)| data)
    }

    pub(crate) fn graphql_with_errors<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<(T, Vec<GqlError>)> {
        let body = serde_json::json!({ "query": query, "variables": variables });
        let mut resp = self.request("graphql", Some(&body), None)?;
        let bytes = resp
            .body_mut()
            .read_to_vec()
            .context("reading GraphQL response")?;
        let env: Envelope<serde_json::Value> =
            serde_json::from_slice(&bytes).context("failed to parse GraphQL JSON")?;
        let secondary = env
            .errors
            .iter()
            .flatten()
            .any(|err| secondary_limit(&err.message));
        let limited = secondary
            || env
                .errors
                .iter()
                .flatten()
                .any(|err| err.kind.as_deref() == Some("RATE_LIMITED"));
        if limited {
            let secondary = secondary
                || (resp.headers().contains_key("retry-after")
                    && resp
                        .headers()
                        .get("x-ratelimit-remaining")
                        .is_none_or(|value| value != "0"));
            self.observe_limit(Resource::Graphql, resp.headers(), secondary)?;
        }
        let data = env.into_data_with_errors();
        if data.is_err() && limited {
            self.check_cooldown(Resource::Graphql)?;
        }
        data
    }

    /// GET a REST path (relative to api.github.com), returning typed JSON.
    pub fn get<T: DeserializeOwned + Serialize>(&self, path: &str) -> Result<T> {
        let cached = self.cached_get(path)?;
        let mut resp =
            self.request(path, None, cached.as_ref().map(|entry| entry.etag.as_str()))?;
        if resp.status().as_u16() == 304 {
            let cached = cached.ok_or_else(|| {
                anyhow!("GitHub returned HTTP 304 for /{path} without a cached response")
            })?;
            if let Some(etag) = resp
                .headers()
                .get("etag")
                .and_then(|value| value.to_str().ok())
            {
                self.cache_get(
                    path,
                    Some(CachedGet {
                        etag: etag.to_string(),
                        body: cached.body.clone(),
                    }),
                )?;
            }
            return serde_json::from_value((*cached.body).clone())
                .with_context(|| format!("parsing cached response from /{path}"));
        }

        let bytes = resp
            .body_mut()
            .read_to_vec()
            .with_context(|| format!("reading response from /{path}"))?;
        let parsed = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing response from /{path}"))?;
        if resp.status().is_success() {
            let etag = resp
                .headers()
                .get("etag")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            if let Some(etag) = etag {
                let body = serde_json::to_value(&parsed)
                    .with_context(|| format!("caching response from /{path}"))?;
                self.cache_get(
                    path,
                    Some(CachedGet {
                        etag,
                        body: Arc::new(body),
                    }),
                )?;
            } else if cached.is_some() {
                self.cache_get(path, None)?;
            }
        }
        Ok(parsed)
    }

    fn cached_get(&self, path: &str) -> Result<Option<CachedGet>> {
        let mut cache = self
            .get_cache
            .lock()
            .map_err(|err| anyhow!("GitHub GET cache unavailable: {err}"))?;
        let Some(entry) = cache.entries.get(path).cloned() else {
            return Ok(None);
        };
        cache.insertion_order.retain(|cached| cached != path);
        cache.insertion_order.push_back(path.to_string());
        Ok(Some(entry))
    }

    fn cache_get(&self, path: &str, entry: Option<CachedGet>) -> Result<()> {
        let mut cache = self
            .get_cache
            .lock()
            .map_err(|err| anyhow!("GitHub GET cache unavailable: {err}"))?;
        if let Some(entry) = entry {
            cache.insert(path.to_string(), entry);
        } else {
            cache.remove(path);
        }
        Ok(())
    }

    #[allow(clippy::unused_self, reason = "tests use this client's fixture clock")]
    fn now(&self) -> DateTime<Utc> {
        #[cfg(test)]
        if let Some(fake) = &self.fake {
            return fake.now();
        }
        Utc::now()
    }

    fn check_cooldown(&self, resource: Resource) -> Result<()> {
        let now = self.now();
        let cooldowns = self
            .cooldowns
            .lock()
            .map_err(|err| anyhow!("GitHub rate-limit state unavailable: {err}"))?;
        if let Some((resource, reset)) = [resource, Resource::Secondary]
            .into_iter()
            .filter_map(|resource| cooldowns.get(&resource).map(|reset| (resource, *reset)))
            .filter(|(_, reset)| *reset > now)
            .max_by_key(|(_, reset)| *reset)
        {
            return Err(resource.error(reset));
        }
        Ok(())
    }

    fn observe_limit(
        &self,
        resource: Resource,
        headers: &HeaderMap,
        secondary: bool,
    ) -> Result<()> {
        let header = |name| headers.get(name).and_then(|value| value.to_str().ok());
        let exhausted = header("x-ratelimit-remaining") == Some("0");
        let reset = header("x-ratelimit-reset")
            .and_then(|value| value.parse().ok())
            .and_then(|seconds| DateTime::from_timestamp(seconds, 0));
        let now = self.now();
        let retry = header("retry-after")
            .and_then(|value| value.parse().ok())
            .and_then(|seconds| TimeDelta::from_std(Duration::from_secs(seconds)).ok())
            .and_then(|delay| now.checked_add_signed(delay));
        let primary_resource = match header("x-ratelimit-resource") {
            Some("core") => Some(Resource::Core),
            Some("graphql") => Some(Resource::Graphql),
            None => Some(resource),
            Some(_) => None,
        };
        let primary = if exhausted {
            primary_resource.zip(reset.max(retry))
        } else {
            None
        };
        let secondary = secondary.then(|| {
            (
                Resource::Secondary,
                retry
                    .or(if exhausted { reset } else { None })
                    .unwrap_or(now + TimeDelta::minutes(1)),
            )
        });
        let mut cooldowns = self
            .cooldowns
            .lock()
            .map_err(|err| anyhow!("GitHub rate-limit state unavailable: {err}"))?;
        for (resource, reset) in primary.into_iter().chain(secondary) {
            cooldowns
                .entry(resource)
                .and_modify(|previous| *previous = (*previous).max(reset))
                .or_insert(reset);
        }
        Ok(())
    }

    fn request(
        &self,
        path: &str,
        body: Option<&serde_json::Value>,
        if_none_match: Option<&str>,
    ) -> Result<Response<ureq::Body>> {
        let resource = if body.is_some() {
            Resource::Graphql
        } else {
            Resource::Core
        };
        self.check_cooldown(resource)?;
        let mut resp = self.send(path, body, if_none_match)?;
        let status = resp.status();
        if status.is_client_error() || status.is_server_error() {
            if matches!(status.as_u16(), 403 | 429) {
                let retry = resp.headers().contains_key("retry-after")
                    && resp
                        .headers()
                        .get("x-ratelimit-remaining")
                        .is_none_or(|value| value != "0");
                self.observe_limit(resource, resp.headers(), retry)?;
                let bytes = resp
                    .body_mut()
                    .read_to_vec()
                    .with_context(|| format!("reading GitHub HTTP {status} error response"))?;
                let secondary =
                    serde_json::from_slice::<serde_json::Value>(&bytes).is_ok_and(|body| {
                        body.get("message")
                            .and_then(|msg| msg.as_str())
                            .is_some_and(secondary_limit)
                            || body
                                .get("errors")
                                .and_then(|errors| errors.as_array())
                                .is_some_and(|errors| {
                                    errors.iter().any(|error| {
                                        error
                                            .get("message")
                                            .and_then(|msg| msg.as_str())
                                            .is_some_and(secondary_limit)
                                    })
                                })
                    });
                if secondary {
                    self.observe_limit(resource, resp.headers(), true)?;
                }
                self.check_cooldown(resource)?;
            }
            return Err(api_err(ureq::Error::StatusCode(status.as_u16())));
        }
        self.observe_limit(resource, resp.headers(), false)?;
        Ok(resp)
    }

    fn send(
        &self,
        path: &str,
        body: Option<&serde_json::Value>,
        if_none_match: Option<&str>,
    ) -> Result<Response<ureq::Body>> {
        #[cfg(test)]
        if let Some(fake) = &self.fake {
            let method = if body.is_some() { "POST" } else { "GET" };
            return fake.respond(format!("{method} /{path}"), if_none_match, body);
        }
        let url = format!("{API}/{path}");
        if let Some(body) = body {
            self.agent
                .post(url)
                .header("Authorization", self.bearer())
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .send_json(body)
                .map_err(api_err)
        } else {
            let request = self
                .agent
                .get(url)
                .header("Authorization", self.bearer())
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28");
            let request = if let Some(etag) = if_none_match {
                request.header("If-None-Match", etag)
            } else {
                request
            };
            request.call().map_err(api_err)
        }
    }

    /// The authenticated user's login. While the REST core quota is exhausted
    /// it asks the separate GraphQL quota instead.
    pub fn me(&self) -> Result<String> {
        #[derive(Deserialize, Serialize)]
        struct User {
            login: String,
        }
        match self.get::<User>("user") {
            Ok(user) => Ok(user.login),
            Err(err)
                if err
                    .downcast_ref::<RateLimited>()
                    .is_some_and(|limit| limit.resource == Resource::Core) =>
            {
                #[derive(Deserialize)]
                struct Viewer {
                    login: String,
                }
                #[derive(Deserialize)]
                struct Data {
                    viewer: Viewer,
                }
                let data: Data =
                    self.graphql("query { viewer { login } }", serde_json::json!({}))?;
                Ok(data.viewer.login)
            }
            Err(err) => Err(err),
        }
    }

    /// The repository's default branch (e.g. `main`).
    pub fn default_branch(&self, repo: &Repo) -> Result<String> {
        #[derive(Deserialize, Serialize)]
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

/// An in-memory GitHub API for tests. `Client::get` and `Client::graphql` ask
/// it instead of the network: it answers `METHOD /path?query` with canned JSON,
/// fails unknown requests like a 404, and logs dispatches past the cooldown gate.
#[cfg(test)]
pub(crate) mod fake {
    use super::Client;
    use anyhow::{Result, anyhow};
    use chrono::{DateTime, Utc};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use ureq::http::Response;

    type ValidatorLog = Vec<(String, Option<String>)>;
    type ScheduledResponses = Arc<Mutex<HashMap<(String, usize), Response<String>>>>;
    type Responder = Arc<dyn Fn(&serde_json::Value) -> String + Send + Sync>;

    #[derive(Clone)]
    pub(crate) struct Fake {
        routes: Arc<Mutex<HashMap<String, Response<String>>>>,
        failures: ScheduledResponses,
        graphql: Arc<Mutex<Option<Responder>>>,
        requests: Arc<Mutex<Vec<String>>>,
        validators: Arc<Mutex<ValidatorLog>>,
        calls: Arc<Mutex<HashMap<String, usize>>>,
        now: Arc<Mutex<DateTime<Utc>>>,
    }

    impl Fake {
        pub(crate) fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }

        pub(crate) fn validators(&self) -> Vec<(String, Option<String>)> {
            self.validators.lock().unwrap().clone()
        }

        pub(crate) fn set_response(
            &self,
            request: &str,
            status: u16,
            headers: &[(&str, &str)],
            body: &str,
        ) {
            self.routes
                .lock()
                .unwrap()
                .insert(request.to_string(), Self::response(status, headers, body));
        }

        pub(crate) fn set_response_on_call(
            &self,
            request: &str,
            call: usize,
            status: u16,
            headers: &[(&str, &str)],
            body: &str,
        ) {
            self.failures.lock().unwrap().insert(
                (request.to_string(), call),
                Self::response(status, headers, body),
            );
        }

        /// Answers every GraphQL POST from its request body, for tests whose
        /// response depends on the variables sent. Takes precedence over routes.
        pub(crate) fn set_graphql_responder(
            &self,
            responder: impl Fn(&serde_json::Value) -> String + Send + Sync + 'static,
        ) {
            *self.graphql.lock().unwrap() = Some(Arc::new(responder));
        }

        fn response(status: u16, headers: &[(&str, &str)], body: &str) -> Response<String> {
            let mut response = Response::builder().status(status);
            for (name, value) in headers {
                response = response.header(*name, *value);
            }
            response.body(body.to_string()).unwrap()
        }

        pub(crate) fn set_now(&self, now: DateTime<Utc>) {
            *self.now.lock().unwrap() = now;
        }

        pub(super) fn now(&self) -> DateTime<Utc> {
            *self.now.lock().unwrap()
        }

        pub(super) fn respond(
            &self,
            request: String,
            if_none_match: Option<&str>,
            body: Option<&serde_json::Value>,
        ) -> Result<Response<ureq::Body>> {
            self.requests.lock().unwrap().push(request.clone());
            self.validators
                .lock()
                .unwrap()
                .push((request.clone(), if_none_match.map(str::to_string)));
            let call = {
                let mut calls = self.calls.lock().unwrap();
                let count = calls.entry(request.clone()).or_default();
                *count += 1;
                *count
            };
            let responder = self.graphql.lock().unwrap().clone();
            if let (Some(responder), Some(body)) = (responder, body) {
                return Ok(
                    Response::new(responder(body)).map(|body| ureq::Body::builder().data(body))
                );
            }
            let response = self
                .failures
                .lock()
                .unwrap()
                .get(&(request.clone(), call))
                .cloned()
                .or_else(|| self.routes.lock().unwrap().get(&request).cloned())
                .ok_or_else(|| anyhow!("GitHub request failed: 404 for {request}"))?;
            Ok(response.map(|body| ureq::Body::builder().data(body)))
        }
    }

    pub(crate) fn client(routes: impl IntoIterator<Item = (String, String)>) -> (Client, Fake) {
        let fake = Fake {
            routes: Arc::new(Mutex::new(
                routes
                    .into_iter()
                    .map(|(route, body)| (route, Response::new(body)))
                    .collect(),
            )),
            failures: Arc::default(),
            graphql: Arc::default(),
            requests: Arc::default(),
            validators: Arc::default(),
            calls: Arc::default(),
            now: Arc::new(Mutex::new(Utc::now())),
        };
        let mut client = Client::new(String::new());
        client.fake = Some(fake.clone());
        (client, fake)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;
    const RESET: &str = "1700000120";
    const REST: &str = "GET /user";
    const GRAPHQL: &str = "POST /graphql";
    const USER: &str = r#"{"login":"me"}"#;
    const DATA: &str = r#"{"data":{"login":"me"}}"#;
    const PRIMARY: &str =
        r#"{"errors":[{"type":"RATE_LIMITED","message":"API rate limit exceeded"}]}"#;
    const SECONDARY: &str = r#"{"message":"You have exceeded a secondary rate limit."}"#;
    const SECONDARY_GQL: &str = r#"{"errors":[{"type":"RATE_LIMITED","message":"You have exceeded a secondary rate limit."}]}"#;

    fn client() -> (Client, fake::Fake) {
        let (client, fake) = fake::client([
            (REST.to_string(), USER.to_string()),
            (GRAPHQL.to_string(), DATA.to_string()),
        ]);
        fake.set_now(DateTime::from_timestamp(NOW, 0).unwrap());
        (client, fake)
    }

    fn graphql(client: &Client) -> Result<serde_json::Value> {
        client.graphql("query { viewer { login } }", serde_json::json!({}))
    }

    #[test]
    fn rest_primary_cooldown_preserves_graphql_and_expires() {
        for status in [403, 429] {
            let (client, fake) = client();
            let clone = client.clone();
            fake.set_response(
                REST,
                status,
                &[
                    ("x-ratelimit-resource", "core"),
                    ("x-ratelimit-remaining", "0"),
                    ("x-ratelimit-reset", RESET),
                ],
                r#"{"message":"API rate limit exceeded"}"#,
            );
            for elapsed in [0, 0, 30, 119] {
                fake.set_now(DateTime::from_timestamp(NOW + elapsed, 0).unwrap());
                let err = clone.get::<serde_json::Value>("user").unwrap_err();
                assert_eq!(
                    err.to_string(),
                    "GitHub core rate limit; retry after 2023-11-14T22:15:20Z",
                    "status={status}, elapsed={elapsed}"
                );
            }
            assert_eq!(fake.requests(), [REST], "status={status}");
            assert_eq!(graphql(&client).unwrap()["login"], "me");
            fake.set_response(REST, 200, &[], USER);
            fake.set_now(DateTime::from_timestamp(NOW + 120, 0).unwrap());
            assert_eq!(clone.me().unwrap(), "me");
            assert_eq!(fake.requests(), [REST, GRAPHQL, REST]);
        }
    }

    #[test]
    fn graphql_primary_cooldown_preserves_rest_and_expires() {
        let (client, fake) = client();
        let clone = client.clone();
        fake.set_response(
            GRAPHQL,
            200,
            &[
                ("x-ratelimit-resource", "graphql"),
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", RESET),
            ],
            PRIMARY,
        );
        for elapsed in [0, 0, 30, 119] {
            fake.set_now(DateTime::from_timestamp(NOW + elapsed, 0).unwrap());
            let err = graphql(&clone).unwrap_err();
            assert_eq!(
                err.to_string(),
                "GitHub graphql rate limit; retry after 2023-11-14T22:15:20Z",
                "elapsed={elapsed}"
            );
        }
        assert_eq!(fake.requests(), [GRAPHQL]);
        assert_eq!(client.me().unwrap(), "me");
        fake.set_response(GRAPHQL, 200, &[], DATA);
        fake.set_now(DateTime::from_timestamp(NOW + 120, 0).unwrap());
        assert_eq!(graphql(&clone).unwrap()["login"], "me");
        assert_eq!(fake.requests(), [GRAPHQL, REST, GRAPHQL]);
        println!(
            "GraphQL RATE_LIMITED: 4 pre-reset calls, 1 dispatch; unrestricted REST and post-reset GraphQL each dispatch once"
        );
    }

    #[test]
    fn secondary_cooldown_spans_both_resources() {
        for (request, status, headers, body, wait) in [
            (
                REST,
                403,
                vec![
                    ("retry-after", "90"),
                    ("x-ratelimit-remaining", "1"),
                    ("x-ratelimit-reset", RESET),
                ],
                SECONDARY,
                90,
            ),
            (GRAPHQL, 200, vec![("retry-after", "90")], SECONDARY_GQL, 90),
            (
                REST,
                429,
                vec![("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", RESET)],
                SECONDARY,
                120,
            ),
            (
                GRAPHQL,
                403,
                vec![("x-ratelimit-remaining", "1"), ("x-ratelimit-reset", RESET)],
                SECONDARY_GQL,
                60,
            ),
            (GRAPHQL, 200, vec![], SECONDARY_GQL, 60),
            (
                REST,
                429,
                vec![("retry-after", "90")],
                "Too many requests",
                90,
            ),
        ] {
            let (client, fake) = client();
            fake.set_response(request, status, &headers, body);
            let err = if request == REST {
                client.get::<serde_json::Value>("user")
            } else {
                graphql(&client)
            }
            .unwrap_err();
            assert!(err.to_string().contains("rate limit"), "{request}: {err}");
            let reset = DateTime::from_timestamp(NOW + wait, 0).unwrap();
            let expected = Resource::Secondary.error(reset).to_string();
            for elapsed in [0, wait - 1] {
                fake.set_now(DateTime::from_timestamp(NOW + elapsed, 0).unwrap());
                for err in [
                    client.clone().me().unwrap_err(),
                    graphql(&client.clone()).unwrap_err(),
                ] {
                    assert_eq!(err.to_string(), expected, "{request}, status={status}");
                }
            }
            assert_eq!(fake.requests(), [request], "{request}, status={status}");
            fake.set_response(REST, 200, &[], USER);
            fake.set_response(GRAPHQL, 200, &[], DATA);
            fake.set_now(reset);
            assert_eq!(client.me().unwrap(), "me");
            assert_eq!(graphql(&client).unwrap()["login"], "me");
            assert_eq!(fake.requests(), [request, REST, GRAPHQL]);
        }
    }

    #[test]
    fn successful_last_response_keeps_its_data() {
        for (request, body) in [(REST, USER), (GRAPHQL, DATA)] {
            let (client, fake) = client();
            fake.set_response(
                request,
                200,
                &[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", RESET)],
                body,
            );
            let result = if request == REST {
                client.get::<serde_json::Value>("user")
            } else {
                graphql(&client)
            };
            assert_eq!(result.unwrap()["login"], "me");
            let blocked = if request == REST {
                client.get::<serde_json::Value>("user")
            } else {
                graphql(&client)
            };
            assert!(blocked.unwrap_err().to_string().contains("rate limit"));
            assert_eq!(fake.requests(), [request]);
        }
    }

    #[test]
    fn conditional_gets_reuse_body_and_replace_etag_across_clones() {
        let (client, fake) = client();
        fake.set_response(REST, 200, &[("etag", "v1")], USER);
        assert_eq!(client.me().unwrap(), "me");

        fake.set_response(REST, 304, &[], "");
        assert_eq!(client.clone().me().unwrap(), "me");

        fake.set_response(REST, 200, &[("etag", "v2")], r#"{"login":"changed"}"#);
        assert_eq!(client.me().unwrap(), "changed");

        fake.set_response(REST, 304, &[], "");
        assert_eq!(client.me().unwrap(), "changed");
        assert_eq!(fake.requests(), [REST; 4]);
        assert_eq!(
            fake.validators(),
            [
                (REST.to_string(), None),
                (REST.to_string(), Some("v1".to_string())),
                (REST.to_string(), Some("v1".to_string())),
                (REST.to_string(), Some("v2".to_string())),
            ]
        );
    }

    #[test]
    fn conditional_get_cache_is_bounded() {
        let routes: Vec<_> = (0..=MAX_CACHED_GETS)
            .map(|index| (format!("GET /resource/{index}"), USER.to_string()))
            .collect();
        let (client, fake) = fake::client(routes.clone());
        for (request, _) in routes.iter().take(MAX_CACHED_GETS) {
            fake.set_response(request, 200, &[("etag", "v1")], USER);
            client
                .get::<serde_json::Value>(request.trim_start_matches("GET /"))
                .unwrap();
        }

        fake.set_response(&routes[0].0, 304, &[], "");
        assert_eq!(
            client.get::<serde_json::Value>("resource/0").unwrap()["login"],
            "me"
        );
        fake.set_response(&routes[MAX_CACHED_GETS].0, 200, &[("etag", "v1")], USER);
        let newest_path = format!("resource/{MAX_CACHED_GETS}");
        client.get::<serde_json::Value>(&newest_path).unwrap();

        assert_eq!(
            client.get::<serde_json::Value>("resource/0").unwrap()["login"],
            "me"
        );
        fake.set_response(&routes[1].0, 304, &[], "");
        assert!(
            client
                .get::<serde_json::Value>("resource/1")
                .unwrap_err()
                .to_string()
                .contains("without a cached response")
        );
        assert_eq!(fake.validators().last(), Some(&(routes[1].0.clone(), None)));
    }

    #[test]
    fn rest_cache_only_retains_parsed_release_fields() {
        #[derive(Debug, Deserialize, PartialEq, Serialize)]
        struct ReleaseFields {
            tag_name: String,
            draft: bool,
            prerelease: bool,
            published_at: Option<String>,
        }

        const PATH: &str = "GET /repos/goreleaser/goreleaser/releases?per_page=50&page=1";
        const BODY_PATH: &str = "repos/goreleaser/goreleaser/releases?per_page=50&page=1";
        let metadata = "x".repeat(1400);
        let releases: Vec<_> = (0..44)
            .map(|release| {
                let count = 59 + usize::from(release < 18);
                let assets: Vec<_> = (0..count)
                    .map(|asset| {
                        serde_json::json!({
                            "name": format!("asset-{release}-{asset}"),
                            "metadata": metadata,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "tag_name": format!("v{release}"),
                    "draft": false,
                    "prerelease": false,
                    "published_at": null,
                    "assets": assets,
                })
            })
            .collect();
        let response = serde_json::to_string(&releases).unwrap();
        let response_bytes = response.len();
        assert!(response_bytes > 3_000_000);
        let (client, fake) = fake::client([(PATH.to_string(), response.clone())]);
        fake.set_response(PATH, 200, &[("etag", "v1")], &response);

        let parsed: Vec<ReleaseFields> = client.get(BODY_PATH).unwrap();
        assert_eq!(parsed.len(), 44);
        assert_eq!(parsed[0].tag_name, "v0");
        assert!(!parsed[0].draft);
        assert!(!parsed[0].prerelease);
        assert_eq!(parsed[0].published_at, None);

        let cache = client.get_cache.lock().unwrap();
        let retained = &cache.entries.get(BODY_PATH).unwrap().body;
        let retained_bytes = serde_json::to_vec(&**retained).unwrap().len();
        assert_eq!(retained.as_array().unwrap().len(), 44);
        assert!(
            retained
                .as_array()
                .unwrap()
                .iter()
                .all(|release| { release.as_object().unwrap().len() == 4 })
        );
        assert!(retained_bytes < 10_000);
        drop(cache);

        fake.set_response(PATH, 304, &[], "");
        assert_eq!(client.get::<Vec<ReleaseFields>>(BODY_PATH).unwrap(), parsed);
        assert_eq!(
            fake.validators().last(),
            Some(&(PATH.to_string(), Some("v1".to_string())))
        );
    }

    #[test]
    fn a_304_still_observes_rate_limits_and_returns_cached_data() {
        let (client, fake) = client();
        fake.set_response(REST, 200, &[("etag", "v1")], USER);
        assert_eq!(client.me().unwrap(), "me");

        fake.set_response(
            REST,
            304,
            &[
                ("x-ratelimit-resource", "core"),
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", RESET),
            ],
            "",
        );
        assert_eq!(client.me().unwrap(), "me");
        assert!(
            client
                .get::<serde_json::Value>("user")
                .unwrap_err()
                .to_string()
                .contains("core rate limit")
        );
        assert_eq!(fake.requests(), [REST, REST]);

        fake.set_now(DateTime::from_timestamp(NOW + 120, 0).unwrap());
        assert_eq!(client.me().unwrap(), "me");
        assert_eq!(fake.requests(), [REST, REST, REST]);
    }

    #[test]
    fn secondary_expiry_does_not_clear_a_longer_primary_limit() {
        let (client, fake) = client();
        fake.set_response(
            REST,
            403,
            &[
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", RESET),
                ("retry-after", "90"),
            ],
            SECONDARY,
        );
        assert!(client.me().is_err());
        assert!(
            graphql(&client)
                .unwrap_err()
                .to_string()
                .contains("secondary")
        );
        assert_eq!(fake.requests(), [REST]);
        fake.set_now(DateTime::from_timestamp(NOW + 90, 0).unwrap());
        assert_eq!(graphql(&client).unwrap()["login"], "me");
        assert!(
            client
                .get::<serde_json::Value>("user")
                .unwrap_err()
                .to_string()
                .contains("core")
        );
        assert_eq!(fake.requests(), [REST, GRAPHQL]);
        fake.set_response(REST, 200, &[], USER);
        fake.set_now(DateTime::from_timestamp(NOW + 120, 0).unwrap());
        assert_eq!(client.me().unwrap(), "me");
        assert_eq!(fake.requests(), [REST, GRAPHQL, REST]);
    }

    #[test]
    fn success_and_low_remaining_counts_do_not_throttle() {
        for remaining in ["1", "4999"] {
            let (client, fake) = client();
            for (request, body) in [(REST, USER), (GRAPHQL, DATA)] {
                fake.set_response(
                    request,
                    200,
                    &[
                        ("x-ratelimit-remaining", remaining),
                        ("x-ratelimit-reset", RESET),
                        ("retry-after", "90"),
                    ],
                    body,
                );
            }
            for _ in 0..2 {
                assert_eq!(client.me().unwrap(), "me");
                assert_eq!(graphql(&client).unwrap()["login"], "me");
            }
            assert_eq!(fake.requests(), [REST, GRAPHQL, REST, GRAPHQL]);
        }
    }

    const VIEWER: &str = r#"{"data":{"viewer":{"login":"me"}}}"#;
    const CORE_LIMITED: &str = r#"{"message":"API rate limit exceeded"}"#;
    const CORE_EXHAUSTED: [(&str, &str); 3] = [
        ("x-ratelimit-resource", "core"),
        ("x-ratelimit-remaining", "0"),
        ("x-ratelimit-reset", RESET),
    ];

    #[test]
    fn healthy_identity_uses_rest_only() {
        let (client, fake) = client();
        assert_eq!(client.me().unwrap(), "me");
        assert_eq!(fake.requests(), [REST]);
    }

    #[test]
    fn core_limit_resolves_identity_through_graphql_without_more_rest() {
        for status in [403, 429] {
            let (client, fake) = client();
            fake.set_response(REST, status, &CORE_EXHAUSTED, CORE_LIMITED);
            fake.set_response(GRAPHQL, 200, &[], VIEWER);
            assert_eq!(client.me().unwrap(), "me", "status={status}");
            assert_eq!(client.clone().me().unwrap(), "me", "status={status}");
            assert_eq!(fake.requests(), [REST, GRAPHQL, GRAPHQL], "status={status}");

            fake.set_response(REST, 200, &[], USER);
            fake.set_now(DateTime::from_timestamp(NOW + 120, 0).unwrap());
            assert_eq!(client.me().unwrap(), "me");
            assert_eq!(fake.requests(), [REST, GRAPHQL, GRAPHQL, REST]);
        }
    }

    #[test]
    fn secondary_limit_does_not_fall_back() {
        let (client, fake) = client();
        fake.set_response(
            REST,
            403,
            &[("retry-after", "90"), ("x-ratelimit-remaining", "1")],
            SECONDARY,
        );
        fake.set_response(GRAPHQL, 200, &[], VIEWER);
        for _ in 0..2 {
            let err = client.me().unwrap_err();
            assert_eq!(
                err.to_string(),
                "GitHub secondary rate limit; retry after 2023-11-14T22:14:50Z"
            );
        }
        assert_eq!(fake.requests(), [REST]);
    }

    #[test]
    fn core_limit_with_a_limited_graphql_bucket_fails_once() {
        let (client, fake) = client();
        fake.set_response(REST, 403, &CORE_EXHAUSTED, CORE_LIMITED);
        fake.set_response(
            GRAPHQL,
            200,
            &[
                ("x-ratelimit-resource", "graphql"),
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", "1700000300"),
            ],
            PRIMARY,
        );
        for _ in 0..2 {
            let err = client.me().unwrap_err();
            assert_eq!(
                err.to_string(),
                "GitHub graphql rate limit; retry after 2023-11-14T22:18:20Z"
            );
        }
        assert_eq!(fake.requests(), [REST, GRAPHQL]);
    }

    #[test]
    fn unauthorized_and_forbidden_identity_do_not_fall_back() {
        for (status, message) in [
            (401, "GitHub rejected the token (401); run `prowl --login`"),
            (403, "GitHub request failed: http status: 403"),
        ] {
            let (client, fake) = client();
            fake.set_response(
                REST,
                status,
                &[
                    ("x-ratelimit-resource", "core"),
                    ("x-ratelimit-remaining", "4000"),
                ],
                r#"{"message":"Forbidden"}"#,
            );
            fake.set_response(GRAPHQL, 200, &[], VIEWER);
            assert_eq!(client.me().unwrap_err().to_string(), message);
            assert_eq!(fake.requests(), [REST], "status={status}");
        }
    }

    #[test]
    fn ordinary_http_errors_keep_their_guidance_and_do_not_throttle() {
        for status in [401, 403, 404, 500] {
            for (request, body) in [(REST, USER), (GRAPHQL, DATA)] {
                let (client, fake) = client();
                fake.set_response(
                    request,
                    status,
                    &[("x-ratelimit-remaining", "1"), ("x-ratelimit-reset", RESET)],
                    r#"{"message":"Resource not accessible by integration"}"#,
                );
                let err = if request == REST {
                    client.get::<serde_json::Value>("user")
                } else {
                    graphql(&client)
                }
                .unwrap_err();
                if status == 401 {
                    assert_eq!(
                        err.to_string(),
                        "GitHub rejected the token (401); run `prowl --login`"
                    );
                } else {
                    assert!(
                        err.to_string().starts_with("GitHub request failed:")
                            && err.to_string().contains(&status.to_string()),
                        "{request}, status={status}: {err}"
                    );
                }
                fake.set_response(request, 200, &[], body);
                let result = if request == REST {
                    client.get::<serde_json::Value>("user")
                } else {
                    graphql(&client)
                };
                assert_eq!(result.unwrap()["login"], "me");
                assert_eq!(fake.requests(), [request, request]);
            }
        }
    }

    #[test]
    fn graphql_errors_and_partial_data_keep_their_semantics() {
        let (client, fake) = client();
        fake.set_response(
            GRAPHQL,
            200,
            &[("x-ratelimit-remaining", "1"), ("x-ratelimit-reset", RESET)],
            r#"{"errors":[{"type":"FORBIDDEN","message":"Resource not accessible by integration"}]}"#,
        );
        assert_eq!(
            graphql(&client).unwrap_err().to_string(),
            "GraphQL error: Resource not accessible by integration"
        );
        fake.set_response(
            GRAPHQL,
            200,
            &[],
            r#"{"data":{"login":"me"},"errors":[{"message":"An optional field is inaccessible"}]}"#,
        );
        assert_eq!(graphql(&client).unwrap()["login"], "me");
        fake.set_response(GRAPHQL, 200, &[], "not JSON");
        assert_eq!(
            graphql(&client).unwrap_err().to_string(),
            "failed to parse GraphQL JSON"
        );
        assert_eq!(fake.requests(), [GRAPHQL; 3]);
    }

    #[test]
    fn primary_limit_without_a_deadline_does_not_guess_one() {
        let (client, fake) = client();
        fake.set_response(GRAPHQL, 200, &[("x-ratelimit-remaining", "0")], PRIMARY);
        for _ in 0..2 {
            assert_eq!(
                graphql(&client).unwrap_err().to_string(),
                "GraphQL error: API rate limit exceeded"
            );
        }
        assert_eq!(fake.requests(), [GRAPHQL; 2]);
    }

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

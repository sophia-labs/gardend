//! Public mode: a self-host that anyone may read (cow-tools.sophia-labs.com,
//! hub plan `plans/2026-10-14-cow-tools-demo.md`, slice W1).
//!
//! A self-hosted gardend is closed: the owner's key, client tokens, Cloudflare
//! Access. Public mode opens a listed set of graphs to callers who bring no
//! credential at all, as viewers, and to people who sign in with a Sophia
//! account (a Cognito ID token), also as viewers. The owner keeps the key and
//! everything it opens.
//!
//! It is off unless `<profile>/public.json` (or `GARDEN_PUBLIC_CONFIG`, or
//! `GARDEN_PUBLIC_GRAPHS`) names at least one graph, it is never on in a hosted
//! cell (`GARDEN_CELL_ID`: the cell boundary is enabled), and only the headless
//! gardend reads the file. Off, nothing here runs: [`public_mode_middleware`]
//! returns at once and [`current_public_request`] is always `None`.
//!
//! What a public caller is:
//!
//! - **anonymous** (no `Authorization`, no Access token, no WebSocket bearer):
//!   `anon:<id>`, the id a hash of the `X-Visitor-Id` header it sends (16 to
//!   128 of `[A-Za-z0-9_-]`, kept by the browser or the agent), else of its
//!   address and user agent. The raw value never leaves gardend.
//! - **an account**: `Authorization: Bearer <Cognito ID token>` verified
//!   against the configured pool (RS256, `iss`, `aud` = the app client id,
//!   `token_use` = `id`, `exp`): `cognito:<sub>`.
//!
//! Either holds [`PUBLIC_SCOPES`] (reads, MCP calls, the service proxy), on the
//! listed graphs only. What gates them, in order:
//!
//! 1. per-address rate limits (a token bucket; `429` with `Retry-After`);
//! 2. a body limit (`413`);
//! 3. the route: only a route every one of whose scopes is public, whose graph
//!    (in its path or its JSON body) is listed, or `/mcp`, the graph list
//!    (filtered), `/health` and the listed services; anything else is `403`
//!    (`public_read_only`) and a graph not listed is `404`;
//! 4. inside: each handler's own scope check (writes need scopes a visitor
//!    never holds), `/mcp`'s graph check (`loopback_mcp_routes`), the service
//!    proxy's (`loopback_service_routes`), and lower SPARQL ceilings
//!    (`sparql_admission`).

use crate::{
    cell_graph_boundary::{json_graph_selectors, path_parameter, PublicRouteClass},
    loopback_state::LoopbackState,
};
use axum::{
    body::Body,
    extract::{ConnectInfo, MatchedPath, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError, RwLock},
    time::{Duration, Instant},
};

/// The file in the profile that turns public mode on.
pub(crate) const PUBLIC_CONFIG_FILE: &str = "public.json";
/// Another path for that file (tests, or a config kept outside the profile).
pub(crate) const PUBLIC_CONFIG_ENV: &str = "GARDEN_PUBLIC_CONFIG";
/// A comma list of graphs; set, it replaces the file's `graphs` (and with no
/// file, turns public mode on with the defaults).
pub(crate) const PUBLIC_GRAPHS_ENV: &str = "GARDEN_PUBLIC_GRAPHS";
/// The front page's folder when `siteDir` is unset (beside `ui/`).
pub(crate) const SITE_DIR_ENV: &str = "GARDEN_SITE_DIR";
pub(crate) const SITE_DIR_NEXT_TO_BINARY: &str = "site";
/// Where the Sophia app is served while a site is served at `/`.
pub(crate) const APP_PATH: &str = "/app/";
/// The site's MCP server card (SEP-2127), served at [`SERVER_CARD_PATH`] too.
pub(crate) const SERVER_CARD_FILE: &str = "mcp-server-card.json";
/// Where SEP-2127 puts an MCP server's card: `<mcp url>/server-card`.
pub(crate) const SERVER_CARD_PATH: &str = "/mcp/server-card";
/// The header a visitor's client sends to be the same visitor across requests.
pub(crate) const VISITOR_ID_HEADER: &str = "x-visitor-id";
/// The header a caller sends its own model key in (forwarded only to services
/// whose manifest says `"acceptsModelKey": true`; dropped everywhere else).
pub(crate) const MODEL_KEY_HEADER: &str = "x-model-key";

/// What a public caller holds: every read a visitor needs to look at a graph
/// (documents, workspace, files, images, wires, entities, salience, search,
/// SPARQL), the MCP door (each tool still checks its own scopes), and the
/// service proxy (only to the listed services). Not: tokens, logs, jobs,
/// profile, history, memory, time travel, RDF dumps, graph exports, or any
/// write.
pub(crate) const PUBLIC_SCOPES: &[&str] = &[
    "artifacts.read",
    "documents.read",
    "entities.read",
    "graphs.read",
    "images.read",
    "loopback.health.read",
    "mcp.tools.call",
    "mcp.tools.read",
    "orientation.read",
    "rdf.query",
    "salience.read",
    "search.lexical.read",
    "search.semantic.read",
    "services.proxy",
    "wires.read",
    "workspace.read",
];

const DEFAULT_PER_MINUTE: f64 = 120.0;
const DEFAULT_BURST: f64 = 60.0;
const DEFAULT_MAX_BODY_BYTES: usize = 32 * 1024;
const DEFAULT_SPARQL_TIMEOUT_MS: u64 = 10_000;
pub(crate) const DEFAULT_SPARQL_MAX_ROWS: usize = 1_000;
const MAX_TRACKED_ADDRESSES: usize = 50_000;
const KEY_REFETCH_INTERVAL: Duration = Duration::from_secs(10);
const KEY_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// `public.json`, as written.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PublicFile {
    #[serde(default)]
    graphs: Vec<String>,
    #[serde(default)]
    services: Vec<String>,
    site_dir: Option<String>,
    client_ip_header: Option<String>,
    #[serde(default)]
    rate_limit: RateLimitFile,
    max_body_bytes: Option<usize>,
    #[serde(default)]
    sparql: SparqlFile,
    accounts: Option<AccountsFile>,
    /// W6: the one folder of a public graph that signed-in accounts may write
    /// in (`visitor_fence`).
    #[serde(default)]
    visitor_folders: Vec<crate::visitor_fence::VisitorFolderFile>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RateLimitFile {
    per_minute: Option<f64>,
    burst: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SparqlFile {
    timeout_ms: Option<u64>,
    max_rows: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccountsFile {
    cognito: CognitoFile,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CognitoFile {
    /// `https://cognito-idp.<region>.amazonaws.com/<pool id>`.
    issuer: String,
    /// The app client id: the ID token's `aud`.
    client_id: String,
    /// The hosted UI's domain (`auth.sophia-labs.com`), for the page's sign-in.
    domain: Option<String>,
    /// Where the hosted UI sends a person back (a page of the site).
    callback_url: Option<String>,
    /// Default `<issuer>/.well-known/jwks.json`.
    jwks_url: Option<String>,
    /// Where the site's sign-in keeps the person's tokens in the browser
    /// (`sessionStorage`/`localStorage`, a JSON object holding `id_token`), so
    /// the app on the same origin signs in with them too (W6).
    token_storage_key: Option<String>,
}

/// Public mode, loaded. `None` anywhere means off.
#[derive(Debug)]
pub(crate) struct PublicMode {
    graphs: BTreeSet<String>,
    services: BTreeSet<String>,
    site_dir: Option<PathBuf>,
    client_ip_header: Option<String>,
    limiter: RateLimiter,
    max_body_bytes: usize,
    sparql: SparqlCeiling,
    accounts: Option<CognitoAccounts>,
    config_source: String,
    /// The site's `mcp-server-card.json`, read at start: served at
    /// `/mcp/server-card`, and `/mcp`'s `serverInfo` names itself by it.
    server_card: Option<serde_json::Value>,
    /// W6: where signed-in accounts may write (`visitor_fence`), if anywhere.
    visitor_folders: Option<crate::visitor_fence::VisitorFolders>,
}

/// The SPARQL ceilings a public caller's queries get (lower than the
/// server's; `sparql_admission::effective_query_limits` takes the smaller).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SparqlCeiling {
    pub(crate) timeout: Duration,
    pub(crate) max_rows: usize,
}

impl PublicMode {
    /// Read public mode for a profile. `Ok(None)`: off (no file and no
    /// `GARDEN_PUBLIC_GRAPHS`, or no graph listed). A file that does not
    /// parse, or a value out of range, is an error: gardend does not start
    /// half-open.
    pub(crate) fn load(
        profile_dir: &Path,
        binary_dir: Option<&Path>,
    ) -> Result<Option<Self>, String> {
        let path = env_value(PUBLIC_CONFIG_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| profile_dir.join(PUBLIC_CONFIG_FILE));
        let file = match std::fs::read(&path) {
            Ok(bytes) => Some(
                serde_json::from_slice::<PublicFile>(&bytes)
                    .map_err(|error| format!("{}: {error}", path.display()))?,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("read {}: {error}", path.display())),
        };
        let graphs_override = env_value(PUBLIC_GRAPHS_ENV);
        if file.is_none() && graphs_override.is_none() {
            return Ok(None);
        }
        let mut file = file.unwrap_or_default();
        if let Some(list) = graphs_override {
            file.graphs = list
                .split(',')
                .map(|graph| graph.trim().to_string())
                .collect();
        }
        let source = path.display().to_string();
        Self::from_file(file, profile_dir, binary_dir, source)
    }

    fn from_file(
        file: PublicFile,
        profile_dir: &Path,
        binary_dir: Option<&Path>,
        config_source: String,
    ) -> Result<Option<Self>, String> {
        let graphs: BTreeSet<String> = file
            .graphs
            .iter()
            .map(|graph| graph.trim())
            .filter(|graph| !graph.is_empty())
            .map(str::to_string)
            .collect();
        for graph in &graphs {
            crate::ids::validate_local_id(graph, "public graph")
                .map_err(|error| format!("public mode: graph {graph:?}: {error}"))?;
        }
        if graphs.is_empty() {
            return Ok(None);
        }
        let services = file
            .services
            .iter()
            .map(|service| service.trim().to_ascii_lowercase())
            .filter(|service| !service.is_empty())
            .collect();
        let site_dir = match file
            .site_dir
            .as_deref()
            .map(str::trim)
            .filter(|dir| !dir.is_empty())
        {
            Some(dir) => {
                let dir = profile_dir.join(dir);
                if !dir.join("index.html").is_file() {
                    return Err(format!(
                        "public mode: siteDir must be a folder holding an index.html: {}",
                        dir.display()
                    ));
                }
                Some(dir)
            }
            None => match env_value(SITE_DIR_ENV) {
                Some(dir) => {
                    let dir = PathBuf::from(dir);
                    if !dir.join("index.html").is_file() {
                        return Err(format!(
                            "{SITE_DIR_ENV} must be a folder holding an index.html: {}",
                            dir.display()
                        ));
                    }
                    Some(dir)
                }
                None => binary_dir
                    .map(|dir| dir.join(SITE_DIR_NEXT_TO_BINARY))
                    .filter(|dir| dir.join("index.html").is_file()),
            },
        };
        let client_ip_header = file
            .client_ip_header
            .map(|name| name.trim().to_ascii_lowercase())
            .filter(|name| !name.is_empty());
        if let Some(name) = &client_ip_header {
            axum::http::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                format!("public mode: clientIpHeader {name:?} is not a header name")
            })?;
        }
        let per_minute = file.rate_limit.per_minute.unwrap_or(DEFAULT_PER_MINUTE);
        let burst = file.rate_limit.burst.unwrap_or(DEFAULT_BURST);
        if !(per_minute.is_finite() && per_minute > 0.0 && burst.is_finite() && burst >= 1.0) {
            return Err(
                "public mode: rateLimit.perMinute must be above 0 and rateLimit.burst at least 1"
                    .into(),
            );
        }
        let max_body_bytes = file
            .max_body_bytes
            .unwrap_or(DEFAULT_MAX_BODY_BYTES)
            .max(1024);
        let sparql = SparqlCeiling {
            timeout: Duration::from_millis(
                file.sparql
                    .timeout_ms
                    .unwrap_or(DEFAULT_SPARQL_TIMEOUT_MS)
                    .max(100),
            ),
            max_rows: file
                .sparql
                .max_rows
                .unwrap_or(DEFAULT_SPARQL_MAX_ROWS)
                .max(1),
        };
        let accounts = file
            .accounts
            .map(|accounts| CognitoAccounts::new(accounts.cognito))
            .transpose()?;
        let visitor_folders = crate::visitor_fence::VisitorFolders::from_file(
            file.visitor_folders,
            &graphs,
            profile_dir,
        )?;
        if visitor_folders.is_some() && accounts.is_none() {
            return Err(
                "public mode: visitorFolders needs accounts (only signed-in accounts write)".into(),
            );
        }
        let server_card = match site_dir.as_ref().map(|dir| dir.join(SERVER_CARD_FILE)) {
            Some(path) if path.is_file() => Some(
                serde_json::from_slice::<serde_json::Value>(
                    &std::fs::read(&path)
                        .map_err(|error| format!("read {}: {error}", path.display()))?,
                )
                .map_err(|error| format!("public mode: {}: {error}", path.display()))?,
            ),
            _ => None,
        };
        Ok(Some(Self {
            graphs,
            services,
            site_dir,
            client_ip_header,
            limiter: RateLimiter::new(per_minute, burst),
            max_body_bytes,
            sparql,
            accounts,
            config_source,
            server_card,
            visitor_folders,
        }))
    }

    /// The MCP server card (SEP-2127) of the site, if it has one.
    pub(crate) fn server_card(&self) -> Option<&serde_json::Value> {
        self.server_card.as_ref()
    }

    /// `serverInfo` for `/mcp`'s `initialize`, as the server card names the
    /// server (`name`, `version`, `title`), when the site has a card.
    pub(crate) fn server_info(&self) -> Option<serde_json::Value> {
        let card = self.server_card.as_ref()?;
        let name = card.get("name")?.as_str()?;
        let mut info = serde_json::json!({
            "name": name,
            "version": card.get("version").and_then(serde_json::Value::as_str).unwrap_or(env!("CARGO_PKG_VERSION")),
        });
        if let Some(title) = card.get("title").and_then(serde_json::Value::as_str) {
            info["title"] = serde_json::Value::String(title.to_string());
        }
        Some(info)
    }

    #[cfg(test)]
    pub(crate) fn for_test(json: serde_json::Value, profile_dir: &Path) -> Self {
        let file: PublicFile = serde_json::from_value(json).expect("public.json for a test");
        Self::from_file(file, profile_dir, None, "test".into())
            .expect("public mode for a test")
            .expect("a test lists at least one graph")
    }

    pub(crate) fn graphs(&self) -> &BTreeSet<String> {
        &self.graphs
    }

    pub(crate) fn is_public_graph(&self, graph_id: &str) -> bool {
        self.graphs.contains(graph_id.trim())
    }

    pub(crate) fn is_public_service(&self, service_id: &str) -> bool {
        self.services.contains(service_id)
    }

    pub(crate) fn site_dir(&self) -> Option<&Path> {
        self.site_dir.as_deref()
    }

    pub(crate) fn accounts(&self) -> Option<&CognitoAccounts> {
        self.accounts.as_ref()
    }

    /// W6: the visitor folders, when `public.json` opens any.
    pub(crate) fn visitor_folders(&self) -> Option<&crate::visitor_fence::VisitorFolders> {
        self.visitor_folders.as_ref()
    }

    pub(crate) fn sparql_ceiling(&self) -> SparqlCeiling {
        self.sparql
    }

    pub(crate) fn config_source(&self) -> &str {
        &self.config_source
    }

    /// The `public` section of `GET /self-host.json`: what a page or an agent
    /// needs to know to use this Sophia without an account.
    pub(crate) fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "graphs": self.graphs,
            "services": self.services,
            "site": self.site_dir.is_some(),
            "app": if self.site_dir.is_some() { APP_PATH } else { "/" },
            "mcp": "/mcp",
            "serverCard": self.server_card.is_some().then_some(SERVER_CARD_PATH),
            "visitorIdHeader": "X-Visitor-Id",
            "modelKeyHeader": "X-Model-Key",
            "limits": {
                "requestsPerMinute": self.limiter.per_minute(),
                "burst": self.limiter.burst,
                "maxBodyBytes": self.max_body_bytes,
                "sparqlTimeoutMs": self.sparql.timeout.as_millis() as u64,
                "sparqlMaxRows": self.sparql.max_rows,
            },
            "accounts": self.accounts.as_ref().map(CognitoAccounts::describe),
            "visitorFolders": self.visitor_folders.as_ref().map(crate::visitor_fence::VisitorFolders::describe),
        })
    }

    /// The address a request comes from: the socket's peer, or, when the peer
    /// is this machine (a proxy in front: Caddy, cloudflared) and
    /// `clientIpHeader` is set, the address that header names (the rightmost
    /// one of `X-Forwarded-For`: the one the proxy itself saw).
    fn client_ip(&self, request: &Request) -> IpAddr {
        let peer = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(address)| address.ip())
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let Some(name) = &self.client_ip_header else {
            return peer;
        };
        if !(peer.is_loopback() || peer.is_unspecified()) {
            return peer;
        }
        request
            .headers()
            .get_all(name.as_str())
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .filter_map(|entry| entry.trim().parse::<IpAddr>().ok())
            .last()
            .unwrap_or(peer)
    }
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

// ─── The rate limiter ────────────────────────────────────────────────────────

/// A token bucket per address: `burst` requests at once, refilled at
/// `per_minute`. Addresses whose bucket is full again are forgotten when the
/// table grows past [`MAX_TRACKED_ADDRESSES`].
#[derive(Debug)]
struct RateLimiter {
    per_second: f64,
    burst: f64,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl RateLimiter {
    fn new(per_minute: f64, burst: f64) -> Self {
        Self {
            per_second: per_minute / 60.0,
            burst,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    fn per_minute(&self) -> f64 {
        self.per_second * 60.0
    }

    /// Take one token for `ip`, or the whole seconds until one is there.
    fn take(&self, ip: IpAddr, now: Instant) -> Result<(), u64> {
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        if buckets.len() > MAX_TRACKED_ADDRESSES {
            let (rate, burst) = (self.per_second, self.burst);
            buckets.retain(|_, bucket| {
                bucket.tokens + now.duration_since(bucket.at).as_secs_f64() * rate < burst
            });
        }
        let bucket = buckets.entry(ip).or_insert(Bucket {
            tokens: self.burst,
            at: now,
        });
        let elapsed = now.saturating_duration_since(bucket.at).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.per_second).min(self.burst);
        bucket.at = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - bucket.tokens) / self.per_second).ceil().max(1.0) as u64)
        }
    }
}

// ─── Sophia accounts (Cognito ID tokens) ─────────────────────────────────────

/// A person signed in with a Sophia account, as their ID token says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountIdentity {
    pub(crate) sub: String,
    pub(crate) email: Option<String>,
    pub(crate) email_verified: bool,
}

#[derive(Debug, Deserialize)]
struct IdTokenClaims {
    sub: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    email_verified: Option<serde_json::Value>,
    #[serde(default)]
    token_use: Option<String>,
}

/// Verifies Cognito ID tokens: RS256 with a key of the pool's JWKS (fetched
/// on first use and when Cognito rotates them, at most every 10 s), `iss` =
/// the pool, `aud` = the app client, `token_use` = `id`, `exp` with 60 s of
/// leeway.
pub(crate) struct CognitoAccounts {
    issuer: String,
    client_id: String,
    domain: Option<String>,
    callback_url: Option<String>,
    jwks_url: String,
    token_storage_key: Option<String>,
    validation: Validation,
    keys: RwLock<HashMap<String, DecodingKey>>,
    last_fetch: tokio::sync::Mutex<Option<Instant>>,
    http: reqwest::Client,
}

impl std::fmt::Debug for CognitoAccounts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CognitoAccounts")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("jwks_url", &self.jwks_url)
            .finish_non_exhaustive()
    }
}

impl CognitoAccounts {
    fn new(file: CognitoFile) -> Result<Self, String> {
        let issuer = file.issuer.trim().trim_end_matches('/').to_string();
        let parsed = url::Url::parse(&issuer)
            .map_err(|error| format!("public mode: accounts.cognito.issuer: {error}"))?;
        if parsed.scheme() != "https" && parsed.host_str() != Some("127.0.0.1") {
            return Err("public mode: accounts.cognito.issuer must be an https URL".into());
        }
        let client_id = file.client_id.trim().to_string();
        if client_id.is_empty() {
            return Err("public mode: accounts.cognito.clientId is empty".into());
        }
        let jwks_url = file
            .jwks_url
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| format!("{issuer}/.well-known/jwks.json"));
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[issuer.as_str()]);
        validation.set_audience(&[client_id.as_str()]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.leeway = 60;
        let http = reqwest::Client::builder()
            .timeout(KEY_FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| format!("build account key client: {error}"))?;
        Ok(Self {
            issuer,
            client_id,
            domain: file
                .domain
                .map(|domain| domain.trim().to_string())
                .filter(|d| !d.is_empty()),
            callback_url: file
                .callback_url
                .map(|url| url.trim().to_string())
                .filter(|url| !url.is_empty()),
            jwks_url,
            token_storage_key: file
                .token_storage_key
                .map(|key| key.trim().to_string())
                .filter(|key| !key.is_empty()),
            validation,
            keys: RwLock::new(HashMap::new()),
            last_fetch: tokio::sync::Mutex::new(None),
            http,
        })
    }

    fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": "cognito",
            "issuer": self.issuer,
            "clientId": self.client_id,
            "domain": self.domain,
            "callbackUrl": self.callback_url,
            "tokenStorageKey": self.token_storage_key,
            "token": "Authorization: Bearer <ID token>",
        })
    }

    /// Verify an ID token against the cached keys (no network).
    pub(crate) fn verify(&self, jwt: &str) -> Result<AccountIdentity, String> {
        let header = jsonwebtoken::decode_header(jwt)
            .map_err(|error| format!("malformed account token: {error}"))?;
        if header.alg != Algorithm::RS256 {
            return Err("account token is not RS256".into());
        }
        let kid = header.kid.ok_or("account token has no kid")?;
        let key = self
            .keys
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&kid)
            .cloned()
            .ok_or_else(|| format!("unknown account signing key {kid}"))?;
        let claims = jsonwebtoken::decode::<IdTokenClaims>(jwt, &key, &self.validation)
            .map_err(|error| format!("invalid account token: {error}"))?
            .claims;
        if claims.token_use.as_deref() != Some("id") {
            return Err("an account token must be an ID token (token_use id)".into());
        }
        let sub = claims.sub.trim().to_string();
        if sub.is_empty()
            || !sub
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        {
            return Err("account token has no usable subject".into());
        }
        let email_verified = match claims.email_verified {
            Some(serde_json::Value::Bool(verified)) => verified,
            Some(serde_json::Value::String(text)) => text.eq_ignore_ascii_case("true"),
            _ => false,
        };
        Ok(AccountIdentity {
            sub,
            email: claims
                .email
                .map(|email| email.trim().to_ascii_lowercase())
                .filter(|email| !email.is_empty()),
            email_verified,
        })
    }

    fn has_key(&self, kid: &str) -> bool {
        self.keys
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(kid)
    }

    /// Make sure the key a token names is cached (never fails: a token whose
    /// key cannot be had does not verify).
    pub(crate) async fn ensure_signing_key(&self, jwt: &str) {
        let Some(kid) = jsonwebtoken::decode_header(jwt)
            .ok()
            .and_then(|header| header.kid)
        else {
            return;
        };
        if self.has_key(&kid) {
            return;
        }
        let mut last_fetch = self.last_fetch.lock().await;
        if self.has_key(&kid) {
            return;
        }
        if last_fetch.is_some_and(|at| at.elapsed() < KEY_REFETCH_INTERVAL) {
            return;
        }
        *last_fetch = Some(Instant::now());
        let fetched = async {
            let response = self
                .http
                .get(self.jwks_url.as_str())
                .send()
                .await
                .map_err(|error| format!("GET {}: {error}", self.jwks_url))?;
            if !response.status().is_success() {
                return Err(format!("GET {}: {}", self.jwks_url, response.status()));
            }
            let body = response
                .bytes()
                .await
                .map_err(|error| format!("GET {}: {error}", self.jwks_url))?;
            self.install_keys(&body)
        }
        .await;
        match fetched {
            Ok(count) => log::info!(
                "public mode: {count} account signing key(s) from {}",
                self.jwks_url
            ),
            Err(error) => log::warn!("public mode: account keys: {error}"),
        }
    }

    /// Replace the cached keys with the RS256 keys of a JWKS document.
    pub(crate) fn install_keys(&self, document: &[u8]) -> Result<usize, String> {
        let document: serde_json::Value = serde_json::from_slice(document)
            .map_err(|error| format!("account keys are not JSON: {error}"))?;
        let entries = document
            .get("keys")
            .and_then(serde_json::Value::as_array)
            .ok_or("account keys document has no keys")?;
        let mut keys = HashMap::new();
        for entry in entries {
            let field = |name: &str| entry.get(name).and_then(serde_json::Value::as_str);
            let (Some(kid), Some("RSA"), Some(n), Some(e)) =
                (field("kid"), field("kty"), field("n"), field("e"))
            else {
                continue;
            };
            if field("alg").is_some_and(|alg| alg != "RS256") {
                continue;
            }
            if let Ok(key) = DecodingKey::from_rsa_components(n, e) {
                keys.insert(kid.to_string(), key);
            }
        }
        if keys.is_empty() {
            return Err("account keys document has no RS256 keys".into());
        }
        let count = keys.len();
        *self.keys.write().unwrap_or_else(PoisonError::into_inner) = keys;
        Ok(count)
    }
}

/// A bearer value shaped like a JWT (three base64url parts): a Sophia account's
/// ID token, never a gardend key or token (`sophia-key-…`, hex, `sophia-dlg-…`).
pub(crate) fn looks_like_jwt(token: &str) -> bool {
    let parts: Vec<&str> = token.split('.').collect();
    parts.len() == 3
        && token.starts_with("eyJ")
        && parts.iter().all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        })
}

// ─── The request's public caller ─────────────────────────────────────────────

/// Who a public request is from, set by [`public_mode_middleware`] for the
/// handler it runs (a task-local: what the handler awaits sees it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublicRequest {
    /// `anon:<hash>`: the visitor, when the request carries no account token.
    pub(crate) visitor_id: String,
    pub(crate) ip: IpAddr,
    /// True when it carries a (JWT-shaped) account token instead.
    pub(crate) account_token: bool,
    pub(crate) sparql: SparqlCeiling,
}

tokio::task_local! {
    static PUBLIC_REQUEST: PublicRequest;
}

/// The public caller of the request this task serves, if it is one.
pub(crate) fn current_public_request() -> Option<PublicRequest> {
    PUBLIC_REQUEST.try_with(Clone::clone).ok()
}

/// The lower SPARQL ceiling of a public caller (`None` for everyone else).
pub(crate) fn current_sparql_ceiling() -> Option<SparqlCeiling> {
    PUBLIC_REQUEST.try_with(|request| request.sparql).ok()
}

/// Run `future` as a public request (a service's MCP call made for a public
/// caller through a delegation token gets the same SPARQL ceilings).
pub(crate) async fn as_public_request<F: std::future::Future>(
    request: PublicRequest,
    future: F,
) -> F::Output {
    PUBLIC_REQUEST.scope(request, future).await
}

/// `anon:` and 24 hex digits: a hash of the visitor id the client sends, else
/// of its address and user agent.
pub(crate) fn visitor_id(headers: &HeaderMap, ip: IpAddr) -> String {
    let sent = headers
        .get(VISITOR_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|id| {
            (16..=128).contains(&id.len())
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        });
    let source = match sent {
        Some(id) => format!("visitor\0{id}"),
        None => {
            let agent = headers
                .get(header::USER_AGENT)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            format!("address\0{ip}\0{agent}")
        }
    };
    let digest = Sha256::digest(source.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("anon:{}", &hex[..24])
}

/// True when the request carries a credential of its own (a bearer, a
/// WebSocket bearer, a Cloudflare Access token): not an anonymous visitor.
fn has_credential(headers: &HeaderMap) -> bool {
    crate::loopback_http::bearer_token(headers).is_some()
        || headers.contains_key(crate::loopback_self_host::ACCESS_JWT_HEADER)
        || headers
            .get("sec-websocket-protocol")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|protocols| {
                protocols
                    .split(',')
                    .any(|entry| entry.trim().starts_with("bearer."))
            })
}

// ─── The gate ────────────────────────────────────────────────────────────────

fn public_error(status: StatusCode, code: &'static str, message: &str) -> Response {
    let mut response = (
        status,
        Json(serde_json::json!({ "ok": false, "code": code, "error": message })),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn read_only(what: &str) -> Response {
    public_error(
        StatusCode::FORBIDDEN,
        "public_read_only",
        &format!("This Sophia is open to read. Visitors cannot {what}."),
    )
}

pub(crate) fn graph_not_public() -> Response {
    public_error(StatusCode::NOT_FOUND, "graph_not_found", "graph not found")
}

enum Gate {
    Allow,
    /// A route whose graph is in its JSON body: read the body to check it.
    CheckBody,
    Deny(Response),
}

fn gate(public: &PublicMode, method: &Method, matched_path: &str, path: &str) -> Gate {
    let method = if method == Method::HEAD {
        Method::GET
    } else {
        method.clone()
    };
    match (&method, matched_path) {
        (&Method::GET | &Method::POST, "/mcp") => return Gate::Allow,
        (_, "/services/{service_id}" | "/services/{service_id}/{*path}") => return Gate::Allow,
        (
            &Method::GET,
            "/graphs" | "/api/graphs" | "/graphs/catalog" | "/api/graphs/catalog" | "/health",
        ) => return Gate::Allow,
        _ => {}
    }
    // What was deleted stays the owner's to see.
    if matched_path.split('/').any(|segment| segment == "trash") {
        return Gate::Deny(read_only("see the trash"));
    }
    let Some(route) = crate::cell_graph_boundary::public_route(&method, matched_path) else {
        return Gate::Deny(read_only("use this route"));
    };
    if !route
        .scopes
        .iter()
        .all(|scope| PUBLIC_SCOPES.contains(&scope.as_str()))
    {
        return Gate::Deny(read_only("change anything"));
    }
    match route.class {
        PublicRouteClass::Global => Gate::Allow,
        PublicRouteClass::PathGraph => match path_parameter(matched_path, path, "{graph_id}") {
            Some(graph)
                if public
                    .is_public_graph(&crate::loopback_self_host_routes::decode_segment(graph)) =>
            {
                Gate::Allow
            }
            _ => Gate::Deny(graph_not_public()),
        },
        PublicRouteClass::BodyGraph => Gate::CheckBody,
        PublicRouteClass::Other => Gate::Deny(read_only("use this route")),
    }
}

/// The router layer of public mode (see the module docs). A request with a
/// credential of its own passes untouched (the owner, members, services,
/// delegations); a static file (no route matched) too.
pub(crate) async fn public_mode_middleware(
    State(state): State<std::sync::Arc<LoopbackState>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(public) = state.self_host.public() else {
        return next.run(request).await;
    };
    let account_token = crate::loopback_http::bearer_token(request.headers())
        .filter(|token| looks_like_jwt(token))
        .filter(|_| public.accounts.is_some())
        .map(str::to_string);
    if account_token.is_none() && has_credential(request.headers()) {
        return next.run(request).await;
    }
    let Some(matched_path) = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_string())
    else {
        return next.run(request).await;
    };
    if request.method() == Method::OPTIONS {
        return next.run(request).await;
    }
    let ip = public.client_ip(&request);
    if let Err(retry_after) = public.limiter.take(ip, Instant::now()) {
        let mut response = public_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            &format!("Too many requests from your address. Try again in {retry_after} s."),
        );
        if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }
    let declared_length = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    if declared_length.is_some_and(|length| length > public.max_body_bytes) {
        return body_too_large(public.max_body_bytes);
    }
    if let (Some(accounts), Some(token)) = (public.accounts(), account_token.as_deref()) {
        accounts.ensure_signing_key(token).await;
    }
    let path = request.uri().path().to_string();
    // W6: a signed-in account's trash and restore of its own documents in the
    // visitors' folder: admitted by its fence, with that route's scopes lent
    // for this one request.
    if let Some(admission) = crate::visitor_fence::rest_admission(
        &state,
        public,
        request.method(),
        &matched_path,
        &path,
        request.uri().query(),
        account_token.as_deref(),
    )
    .await
    {
        return match admission {
            Ok(context) => {
                let visitor = PublicRequest {
                    visitor_id: visitor_id(request.headers(), ip),
                    ip,
                    account_token: account_token.is_some(),
                    sparql: public.sparql,
                };
                PUBLIC_REQUEST
                    .scope(
                        visitor,
                        crate::visitor_fence::within_context(context, next.run(request)),
                    )
                    .await
            }
            Err((status, code, message)) => public_error(status, code, &message),
        };
    }
    let request = match gate(public, request.method(), &matched_path, &path) {
        Gate::Allow => request,
        Gate::Deny(response) => return response,
        Gate::CheckBody => {
            let (parts, body) = request.into_parts();
            let bytes = match axum::body::to_bytes(body, public.max_body_bytes).await {
                Ok(bytes) => bytes,
                Err(_) => return body_too_large(public.max_body_bytes),
            };
            let value: serde_json::Value = match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(error) => {
                    return public_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_json",
                        &format!("the request body is not JSON: {error}"),
                    )
                }
            };
            match json_graph_selectors(&value) {
                Ok(graphs)
                    if !graphs.is_empty()
                        && graphs.iter().all(|graph| public.is_public_graph(graph)) => {}
                Ok(graphs) if graphs.is_empty() => {
                    return public_error(
                        StatusCode::BAD_REQUEST,
                        "graph_required",
                        "name the graph (graphId): one of the public graphs in /self-host.json",
                    )
                }
                Ok(_) => return graph_not_public(),
                Err(error) => return error.http_response(),
            }
            Request::from_parts(parts, Body::from(bytes))
        }
    };
    // What is in the Trash is the owner's to see (W6, sealed exam C1).
    if let Some(why) =
        crate::visitor_fence::trashed_route_refusal(&state.app, &matched_path, &path).await
    {
        return public_error(StatusCode::NOT_FOUND, "not_found", &why);
    }
    let visitor = PublicRequest {
        visitor_id: visitor_id(request.headers(), ip),
        ip,
        account_token: account_token.is_some(),
        sparql: public.sparql,
    };
    PUBLIC_REQUEST.scope(visitor, next.run(request)).await
}

fn body_too_large(limit: usize) -> Response {
    public_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        "body_too_large",
        &format!("A visitor's request body is at most {limit} bytes."),
    )
}

/// Keep the graph list to the public graphs, each as the viewer sees it.
pub(crate) fn public_graph_entries(
    public: &PublicMode,
    entries: Vec<serde_json::Value>,
) -> Vec<serde_json::Value> {
    entries
        .into_iter()
        .filter(|entry| {
            entry
                .get("graph_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|graph| public.is_public_graph(graph))
        })
        .map(|mut entry| {
            if let Some(object) = entry.as_object_mut() {
                object.insert("role".into(), serde_json::json!("viewer"));
            }
            entry
        })
        .collect()
}

// The test modules carry `loopback_self_host` in their names: public mode is
// part of the self-host surface, and the cells lane's gating passes select
// tests by that name (`ship/lanes/cells.sh`).
#[cfg(all(test, feature = "headless"))]
#[path = "public_mode_tests.rs"]
mod loopback_self_host_public_mode_tests;

#[cfg(test)]
mod loopback_self_host_public_mode_unit {
    use super::*;

    #[test]
    fn the_bucket_trips_and_recovers() {
        let limiter = RateLimiter::new(60.0, 3.0);
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        let other: IpAddr = "203.0.113.8".parse().unwrap();
        let start = Instant::now();
        for _ in 0..3 {
            assert!(limiter.take(ip, start).is_ok());
        }
        assert_eq!(limiter.take(ip, start), Err(1));
        assert!(
            limiter.take(other, start).is_ok(),
            "buckets are per address"
        );
        assert!(limiter
            .take(ip, start + Duration::from_millis(1_100))
            .is_ok());
        assert_eq!(
            limiter.take(ip, start + Duration::from_millis(1_100)),
            Err(1)
        );
    }

    #[test]
    fn a_visitor_is_its_id_else_its_address_and_agent() {
        let ip: IpAddr = "198.51.100.4".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::USER_AGENT, "curl/8".parse().unwrap());
        let by_address = visitor_id(&headers, ip);
        assert!(by_address.starts_with("anon:") && by_address.len() == 29);
        assert_eq!(visitor_id(&headers, ip), by_address);
        assert_ne!(
            visitor_id(&headers, "198.51.100.5".parse().unwrap()),
            by_address
        );
        headers.insert(VISITOR_ID_HEADER, "short".parse().unwrap());
        assert_eq!(
            visitor_id(&headers, ip),
            by_address,
            "a short id is not one"
        );
        headers.insert(
            VISITOR_ID_HEADER,
            "a-visitor-of-the-cows-0001".parse().unwrap(),
        );
        let by_id = visitor_id(&headers, ip);
        assert_ne!(by_id, by_address);
        assert_eq!(
            visitor_id(&headers, "192.0.2.9".parse().unwrap()),
            by_id,
            "the id, wherever it comes from"
        );
        assert!(!by_id.contains("cows"), "the raw id never shows");
    }

    #[test]
    fn jwt_shapes() {
        assert!(looks_like_jwt(
            "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ4In0.c2lnbmF0dXJl"
        ));
        assert!(!looks_like_jwt("sophia-key-0123456789abcdef"));
        assert!(!looks_like_jwt("sophia-dlg-0123.abc.def"));
        assert!(!looks_like_jwt("eyJ.only"));
    }

    #[test]
    fn a_file_without_graphs_is_off_and_a_typo_is_an_error() {
        let dir = std::env::temp_dir().join(format!("garden-public-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file: PublicFile = serde_json::from_value(serde_json::json!({ "graphs": [] })).unwrap();
        assert!(PublicMode::from_file(file, &dir, None, "t".into())
            .unwrap()
            .is_none());
        assert!(
            serde_json::from_value::<PublicFile>(serde_json::json!({ "graph": ["a"] })).is_err()
        );
        let file: PublicFile =
            serde_json::from_value(serde_json::json!({ "graphs": ["../etc"] })).unwrap();
        assert!(PublicMode::from_file(file, &dir, None, "t".into()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

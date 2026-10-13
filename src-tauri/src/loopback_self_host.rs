//! Self-host mode for the loopback server (hub plan `plans/2026-10-12-self-host.md`,
//! workstream A).
//!
//! Someone running gardend on their own machine, reached through a Cloudflare
//! Tunnel with Cloudflare Access in front, needs four things the hosted cells
//! never use:
//!
//! - the UI served from the same origin as the API (`loopback_self_host_routes`);
//! - the signed identity token Access attaches to every request
//!   (`Cf-Access-Jwt-Assertion`) accepted as a login ([`CfAccess`]);
//! - their public hostname accepted as an `Origin`, with CORS narrowed to the
//!   origins the server accepts anyway ([`SelfHost::origin_listed`],
//!   [`self_host_cors_layer`]);
//! - a small public description of the mode, `GET /self-host.json`.
//!
//! Every piece is off unless one of its knobs is set. With none set,
//! [`SelfHost::is_active`] is false and the loopback server is exactly what it
//! was before this module existed: hosted cells on Sirin set none of them.
//!
//! | Knob | Meaning |
//! |---|---|
//! | `GARDEN_UI_DIR` | Static UI build to serve at `/`. Unset: a `ui/` folder next to the gardend binary, if it holds an `index.html`. |
//! | `GARDEN_CF_ACCESS_TEAM_DOMAIN` | e.g. `myteam.cloudflareaccess.com`. Set together with the AUD. |
//! | `GARDEN_CF_ACCESS_AUD` | The Access application's AUD tag (a comma list is accepted). |
//! | `GARDEN_CF_ACCESS_ADMIN_EMAILS` | Optional comma list. Set: those people get `session-all`, everyone else Access admits gets the member grant ([`MEMBER_GRANT`]: every read, the `authoring` writes, MCP calls). Unset: everyone Access admits gets `session-all`. |
//! | `GARDEN_ALLOWED_ORIGINS` | Comma list of extra origins to accept. Unset with Access configured: the origin of `GARDEN_PUBLIC_BASE_URL`. |

use crate::loopback_token_grants::{session_all_token_grant, MEMBER_GRANT_PROFILE};
use axum::http::{header, request::Parts, HeaderMap, HeaderName, HeaderValue};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, PoisonError, RwLock},
    time::{Duration, Instant},
};
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};

pub(crate) const UI_DIR_ENV: &str = "GARDEN_UI_DIR";
pub(crate) const CF_ACCESS_TEAM_DOMAIN_ENV: &str = "GARDEN_CF_ACCESS_TEAM_DOMAIN";
pub(crate) const CF_ACCESS_AUD_ENV: &str = "GARDEN_CF_ACCESS_AUD";
pub(crate) const CF_ACCESS_ADMIN_EMAILS_ENV: &str = "GARDEN_CF_ACCESS_ADMIN_EMAILS";
pub(crate) const ALLOWED_ORIGINS_ENV: &str = "GARDEN_ALLOWED_ORIGINS";
pub(crate) const PUBLIC_BASE_URL_ENV: &str = "GARDEN_PUBLIC_BASE_URL";
/// The folder next to the gardend binary used when `GARDEN_UI_DIR` is unset
/// (the release archives carry it; contract "Default UI location").
pub(crate) const UI_DIR_NEXT_TO_BINARY: &str = "ui";
/// The header Cloudflare Access adds to every request it lets through.
pub(crate) const ACCESS_JWT_HEADER: &str = "cf-access-jwt-assertion";
/// A fetch of the team's keys triggered by an unknown `kid` happens at most
/// this often, so a stream of forged tokens cannot turn into a stream of
/// requests to Cloudflare.
const ACCESS_KEY_REFETCH_INTERVAL: Duration = Duration::from_secs(10);
const ACCESS_KEY_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The raw knob values, read once from the environment (or given by a test).
#[derive(Debug, Default, Clone)]
pub(crate) struct SelfHostSettings {
    pub(crate) ui_dir: Option<String>,
    /// Where the running binary lives; the default `ui/` is looked for here.
    pub(crate) binary_dir: Option<PathBuf>,
    pub(crate) cf_access_team_domain: Option<String>,
    pub(crate) cf_access_aud: Option<String>,
    pub(crate) cf_access_admin_emails: Option<String>,
    pub(crate) allowed_origins: Option<String>,
    pub(crate) public_base_url: Option<String>,
}

impl SelfHostSettings {
    pub(crate) fn from_env() -> Self {
        Self {
            ui_dir: env_value(UI_DIR_ENV),
            binary_dir: binary_dir(),
            cf_access_team_domain: env_value(CF_ACCESS_TEAM_DOMAIN_ENV),
            cf_access_aud: env_value(CF_ACCESS_AUD_ENV),
            cf_access_admin_emails: env_value(CF_ACCESS_ADMIN_EMAILS_ENV),
            allowed_origins: env_value(ALLOWED_ORIGINS_ENV),
            public_base_url: env_value(PUBLIC_BASE_URL_ENV),
        }
    }
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// The directory of the running gardend binary, symlinks resolved (a package
/// manager's `bin/gardend` link points into the folder that carries `ui/`).
/// Only the headless binary looks here: the desktop app ships its own UI.
#[cfg(feature = "headless")]
fn binary_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    exe.parent().map(Path::to_path_buf)
}

#[cfg(not(feature = "headless"))]
fn binary_dir() -> Option<PathBuf> {
    None
}

/// Self-host configuration of one loopback server. `Default` is "off".
#[derive(Debug, Default)]
pub(crate) struct SelfHost {
    ui_dir: Option<PathBuf>,
    access: Option<CfAccess>,
    allowed_origins: Vec<String>,
    active: bool,
}

impl SelfHost {
    pub(crate) fn from_env() -> Result<Self, String> {
        Self::from_settings(SelfHostSettings::from_env())
    }

    pub(crate) fn from_settings(settings: SelfHostSettings) -> Result<Self, String> {
        let ui_dir = resolve_ui_dir(settings.ui_dir.as_deref(), settings.binary_dir.as_deref())?;
        let access = match (
            settings.cf_access_team_domain.as_deref(),
            settings.cf_access_aud.as_deref(),
        ) {
            (Some(team_domain), Some(aud)) => Some(CfAccess::new(
                team_domain,
                aud,
                settings.cf_access_admin_emails.as_deref(),
            )?),
            (None, None) => {
                if settings.cf_access_admin_emails.is_some() {
                    return Err(format!(
                        "{CF_ACCESS_ADMIN_EMAILS_ENV} needs {CF_ACCESS_TEAM_DOMAIN_ENV} and {CF_ACCESS_AUD_ENV}"
                    ));
                }
                None
            }
            _ => {
                return Err(format!(
                    "{CF_ACCESS_TEAM_DOMAIN_ENV} and {CF_ACCESS_AUD_ENV} must be set together"
                ))
            }
        };
        let mut allowed_origins = match settings.allowed_origins.as_deref() {
            Some(list) => parse_origin_list(list)?,
            None => match (&access, settings.public_base_url.as_deref()) {
                (Some(_), Some(base)) => vec![normalize_origin(base).ok_or_else(|| {
                    format!("{PUBLIC_BASE_URL_ENV} has no http(s) origin: {base}")
                })?],
                _ => Vec::new(),
            },
        };
        allowed_origins.sort();
        allowed_origins.dedup();
        let active = ui_dir.is_some() || access.is_some() || settings.allowed_origins.is_some();
        Ok(Self {
            ui_dir,
            access,
            allowed_origins,
            active,
        })
    }

    /// True when any self-host knob is in effect. False means the loopback
    /// server behaves exactly as it did before self-host existed.
    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    pub(crate) fn ui_dir(&self) -> Option<&Path> {
        self.ui_dir.as_deref()
    }

    pub(crate) fn access(&self) -> Option<&CfAccess> {
        self.access.as_ref()
    }

    pub(crate) fn allowed_origins(&self) -> &[String] {
        &self.allowed_origins
    }

    /// `"cloudflare-access"` when Access is configured, else `"key"`
    /// (the `auth` field of `GET /self-host.json`).
    pub(crate) fn auth_mode(&self) -> &'static str {
        if self.access.is_some() {
            "cloudflare-access"
        } else {
            "key"
        }
    }

    /// True when `origin` is one of the configured allowed origins.
    pub(crate) fn origin_listed(&self, origin: &str) -> bool {
        if self.allowed_origins.is_empty() {
            return false;
        }
        normalize_origin(origin).is_some_and(|origin| {
            self.allowed_origins
                .iter()
                .any(|allowed| *allowed == origin)
        })
    }

    /// The origin rule for a request authenticated by an ambient credential
    /// (the Access JWT rides the `CF_Authorization` cookie at the edge): no
    /// `Origin` (not a cross-origin browser request) or a loopback, Tauri or
    /// listed origin, never `null`, and never a request the browser itself
    /// marks `Sec-Fetch-Site: cross-site`. Bearer tokens are not ambient and
    /// keep the plain [`crate::loopback_http::origin_ok`] rule.
    pub(crate) fn ambient_origin_ok(&self, headers: &HeaderMap) -> bool {
        if headers
            .get("sec-fetch-site")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|site| site.eq_ignore_ascii_case("cross-site"))
        {
            return false;
        }
        match headers.get(header::ORIGIN) {
            None => true,
            Some(value) => value.to_str().is_ok_and(|origin| {
                origin != "null"
                    && (crate::loopback_http::local_origin(origin) || self.origin_listed(origin))
            }),
        }
    }

    /// The Access login a request carries. `None`: Access is not configured or
    /// the request has no Access token. `Some(Err)`: a token that does not
    /// verify. The signing key must already be cached; the router's
    /// `access_signing_key_middleware` fetches unknown keys before handlers run.
    pub(crate) fn access_grant(&self, headers: &HeaderMap) -> Option<Result<AccessGrant, String>> {
        let access = self.access.as_ref()?;
        let jwt = access_jwt(headers)?;
        Some(access.verify(jwt))
    }
}

fn resolve_ui_dir(
    explicit: Option<&str>,
    binary_dir: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
    if let Some(dir) = explicit {
        let dir = PathBuf::from(dir);
        if !dir.join("index.html").is_file() {
            return Err(format!(
                "{UI_DIR_ENV} must be a folder holding an index.html: {}",
                dir.display()
            ));
        }
        return Ok(Some(dir));
    }
    Ok(binary_dir
        .map(|dir| dir.join(UI_DIR_NEXT_TO_BINARY))
        .filter(|dir| dir.join("index.html").is_file()))
}

/// `scheme://host[:port]` of an http(s) URL or origin, lowercased the way
/// browsers serialise the `Origin` header; `None` for anything else.
pub(crate) fn normalize_origin(value: &str) -> Option<String> {
    let url = url::Url::parse(value.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let origin = url.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

fn parse_origin_list(list: &str) -> Result<Vec<String>, String> {
    list.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            normalize_origin(entry)
                .ok_or_else(|| format!("{ALLOWED_ORIGINS_ENV}: not an http(s) origin: {entry}"))
        })
        .collect()
}

/// The CORS layer while self-host is active: the same shape as
/// `loopback_cors_layer` (credentials, mirrored methods and headers, the
/// exposed incarnation headers), but an `Origin` is answered only when the
/// server would accept it anyway: loopback, Tauri or listed. `null` is never
/// answered.
pub(crate) fn self_host_cors_layer(
    self_host: Arc<SelfHost>,
    exposed: &[&'static str],
) -> CorsLayer {
    CorsLayer::new()
        .allow_credentials(true)
        .allow_headers(AllowHeaders::mirror_request())
        .allow_methods(AllowMethods::mirror_request())
        .allow_origin(AllowOrigin::predicate(
            move |origin: &HeaderValue, _request: &Parts| {
                origin.to_str().is_ok_and(|origin| {
                    origin != "null"
                        && (crate::loopback_http::local_origin(origin)
                            || self_host.origin_listed(origin))
                })
            },
        ))
        .expose_headers(
            exposed
                .iter()
                .copied()
                .map(HeaderName::from_static)
                .collect::<Vec<_>>(),
        )
}

/// The `Cf-Access-Jwt-Assertion` value, if any.
pub(crate) fn access_jwt(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(ACCESS_JWT_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Who a verified Access token speaks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AccessIdentity {
    /// A person Access admitted (email one-time PIN, Google, GitHub, ...).
    Person { email: String },
    /// A Cloudflare service token: `sub` is empty and `common_name` is the
    /// token's Client ID.
    Service { client_id: String },
}

/// The grant of a listed admin, and of everyone Access admits when no admin
/// list is set.
pub(crate) const ADMIN_GRANT: &str = "session-all";
/// The grant of everyone else Access admits while an admin list is set (and
/// of service tokens then): the `member` grant profile
/// ([`MEMBER_GRANT_PROFILE`]: every read scope, the `authoring` profile's
/// writes, and MCP calls, each tool still checking its own scopes), the same
/// one `gardend token create --grant member` mints. `authoring` alone could
/// not reach `/mcp` at all and lacked most reads, which left the UI unusable.
pub(crate) const MEMBER_GRANT: &str = MEMBER_GRANT_PROFILE;

/// A verified Access login and the grant it maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccessGrant {
    pub(crate) identity: AccessIdentity,
    /// [`ADMIN_GRANT`] or [`MEMBER_GRANT`].
    pub(crate) grant_profile: &'static str,
}

impl AccessGrant {
    pub(crate) fn scopes(&self) -> Vec<String> {
        access_grant_scopes(self.grant_profile)
    }

    pub(crate) fn email(&self) -> Option<&str> {
        match &self.identity {
            AccessIdentity::Person { email } => Some(email),
            AccessIdentity::Service { .. } => None,
        }
    }
}

/// The scopes of a grant profile (`loopback_token_grants`), the same lists a
/// client token minted with that profile carries.
fn access_grant_scopes(grant: &str) -> Vec<String> {
    static GRANTS: OnceLock<HashMap<&'static str, Vec<String>>> = OnceLock::new();
    GRANTS
        .get_or_init(|| {
            session_all_token_grant()
                .grant_profiles
                .into_iter()
                .map(|profile| {
                    (
                        profile.id,
                        profile.scopes.into_iter().map(str::to_string).collect(),
                    )
                })
                .collect()
        })
        .get(grant)
        .cloned()
        .unwrap_or_default()
}

#[derive(Debug, Deserialize)]
struct AccessClaims {
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    common_name: Option<String>,
}

/// Cloudflare Access as a login: verifies `Cf-Access-Jwt-Assertion` (RS256,
/// the team's keys from `https://<team>/cdn-cgi/access/certs` matched by
/// `kid`, `iss` = `https://<team>`, `aud` = the application's AUD tag, `exp`
/// and `nbf` with a 60 s leeway) and maps it to a grant profile.
pub(crate) struct CfAccess {
    team_domain: String,
    certs_url: String,
    admin_emails: Option<BTreeSet<String>>,
    validation: Validation,
    keys: RwLock<HashMap<String, DecodingKey>>,
    last_fetch: tokio::sync::Mutex<Option<Instant>>,
    refetch_interval: Duration,
    http: reqwest::Client,
}

impl std::fmt::Debug for CfAccess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CfAccess")
            .field("team_domain", &self.team_domain)
            .field("certs_url", &self.certs_url)
            .field("admin_emails", &self.admin_emails)
            .finish_non_exhaustive()
    }
}

impl CfAccess {
    pub(crate) fn new(
        team_domain: &str,
        aud: &str,
        admin_emails: Option<&str>,
    ) -> Result<Self, String> {
        let team_domain = team_domain
            .trim()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_ascii_lowercase();
        if team_domain.is_empty()
            || team_domain.contains(['/', ' ', '?', '#', '@'])
            || !team_domain.contains('.')
        {
            return Err(format!(
                "{CF_ACCESS_TEAM_DOMAIN_ENV} must be a host name such as myteam.cloudflareaccess.com, got {team_domain:?}"
            ));
        }
        let audiences = aud
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if audiences.is_empty() {
            return Err(format!("{CF_ACCESS_AUD_ENV} is empty"));
        }
        let admin_emails = admin_emails
            .map(|list| {
                list.split(',')
                    .map(|entry| entry.trim().to_ascii_lowercase())
                    .filter(|entry| !entry.is_empty())
                    .collect::<BTreeSet<_>>()
            })
            .filter(|emails| !emails.is_empty());
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[format!("https://{team_domain}")]);
        validation.set_audience(&audiences);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        validation.validate_nbf = true;
        let http = reqwest::Client::builder()
            .timeout(ACCESS_KEY_FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| format!("build Cloudflare Access key client: {error}"))?;
        Ok(Self {
            certs_url: format!("https://{team_domain}/cdn-cgi/access/certs"),
            team_domain,
            admin_emails,
            validation,
            keys: RwLock::new(HashMap::new()),
            last_fetch: tokio::sync::Mutex::new(None),
            refetch_interval: ACCESS_KEY_REFETCH_INTERVAL,
            http,
        })
    }

    pub(crate) fn team_domain(&self) -> &str {
        &self.team_domain
    }

    pub(crate) fn admin_emails_configured(&self) -> bool {
        self.admin_emails.is_some()
    }

    /// Verify a token against the cached keys (no network).
    pub(crate) fn verify(&self, jwt: &str) -> Result<AccessGrant, String> {
        let header = jsonwebtoken::decode_header(jwt)
            .map_err(|error| format!("malformed Cloudflare Access token: {error}"))?;
        if header.alg != Algorithm::RS256 {
            return Err(format!(
                "Cloudflare Access token is {:?}, not RS256",
                header.alg
            ));
        }
        let kid = header
            .kid
            .ok_or_else(|| "Cloudflare Access token has no kid".to_string())?;
        let key = self
            .keys
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&kid)
            .cloned()
            .ok_or_else(|| format!("unknown Cloudflare Access signing key {kid}"))?;
        let claims = jsonwebtoken::decode::<AccessClaims>(jwt, &key, &self.validation)
            .map_err(|error| format!("invalid Cloudflare Access token: {error}"))?
            .claims;
        let identity = identity_from_claims(claims)?;
        let grant_profile = self.grant_profile_for(&identity);
        Ok(AccessGrant {
            identity,
            grant_profile,
        })
    }

    fn grant_profile_for(&self, identity: &AccessIdentity) -> &'static str {
        match (&self.admin_emails, identity) {
            (None, _) => ADMIN_GRANT,
            (Some(admins), AccessIdentity::Person { email }) if admins.contains(email) => {
                ADMIN_GRANT
            }
            (Some(_), _) => MEMBER_GRANT,
        }
    }

    fn has_key(&self, kid: &str) -> bool {
        self.keys
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(kid)
    }

    /// Make sure the key a token names is cached: fetch the team's keys when
    /// its `kid` is unknown (first use, or after Cloudflare rotates them),
    /// at most once per refetch interval. Never fails; a token whose key
    /// cannot be had simply does not verify.
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
        if let Some(at) = *last_fetch {
            if at.elapsed() < self.refetch_interval {
                return;
            }
        }
        *last_fetch = Some(Instant::now());
        match self.fetch_keys().await {
            Ok(count) => log::info!(
                "self-host: {count} Cloudflare Access signing key(s) from {}",
                self.certs_url
            ),
            Err(error) => log::warn!("self-host: Cloudflare Access keys: {error}"),
        }
    }

    async fn fetch_keys(&self) -> Result<usize, String> {
        let response = self
            .http
            .get(self.certs_url.as_str())
            .send()
            .await
            .map_err(|error| format!("GET {}: {error}", self.certs_url))?;
        if !response.status().is_success() {
            return Err(format!("GET {}: {}", self.certs_url, response.status()));
        }
        let body = response
            .bytes()
            .await
            .map_err(|error| format!("GET {}: {error}", self.certs_url))?;
        self.install_keys(&body)
    }

    /// Replace the cached keys with the RS256 keys of a JWKS document
    /// (`{"keys":[{"kid","kty":"RSA","n","e",...}]}`; Cloudflare's extra
    /// `public_cert(s)` fields are ignored). Keys that rotated out disappear.
    pub(crate) fn install_keys(&self, document: &[u8]) -> Result<usize, String> {
        let document: serde_json::Value = serde_json::from_slice(document)
            .map_err(|error| format!("Cloudflare Access keys are not JSON: {error}"))?;
        let entries = document
            .get("keys")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| "Cloudflare Access keys document has no keys".to_string())?;
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
            match DecodingKey::from_rsa_components(n, e) {
                Ok(key) => {
                    keys.insert(kid.to_string(), key);
                }
                Err(error) => log::warn!("self-host: skipping Access key {kid}: {error}"),
            }
        }
        if keys.is_empty() {
            return Err("Cloudflare Access keys document has no RS256 keys".to_string());
        }
        let count = keys.len();
        *self.keys.write().unwrap_or_else(PoisonError::into_inner) = keys;
        Ok(count)
    }
}

fn identity_from_claims(claims: AccessClaims) -> Result<AccessIdentity, String> {
    let non_empty = |value: Option<String>| {
        value
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    if non_empty(claims.sub).is_none() {
        let client_id = non_empty(claims.common_name).ok_or_else(|| {
            "Cloudflare Access token has neither a subject nor a service-token common_name"
                .to_string()
        })?;
        return Ok(AccessIdentity::Service { client_id });
    }
    let email = non_empty(claims.email)
        .ok_or_else(|| "Cloudflare Access token has a subject but no email".to_string())?;
    Ok(AccessIdentity::Person {
        email: email.to_ascii_lowercase(),
    })
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A real RSA key pair (generated per test run), its JWKS document, and
    //! RS256 tokens signed with it in Cloudflare Access's claim shape.
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use jsonwebtoken::{EncodingKey, Header};
    use rsa::{pkcs1::EncodeRsaPrivateKey, traits::PublicKeyParts, RsaPrivateKey};

    pub(crate) const TEAM: &str = "selfhost-test.cloudflareaccess.com";
    pub(crate) const AUD: &str = "4714c1358e65fe4b408ad6d432a5f878f08194bdb4752441fd56faefa9b2b6f2";

    pub(crate) struct TestKey {
        pub(crate) kid: String,
        encoding: EncodingKey,
        n: String,
        e: String,
    }

    impl TestKey {
        pub(crate) fn generate(kid: &str) -> Self {
            let private =
                RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).expect("generate RSA key");
            let der = private.to_pkcs1_der().expect("encode RSA key");
            Self {
                kid: kid.to_string(),
                encoding: EncodingKey::from_rsa_der(der.as_bytes()),
                n: URL_SAFE_NO_PAD.encode(private.n().to_bytes_be()),
                e: URL_SAFE_NO_PAD.encode(private.e().to_bytes_be()),
            }
        }

        pub(crate) fn jwk(&self) -> serde_json::Value {
            serde_json::json!({
                "kid": self.kid, "kty": "RSA", "alg": "RS256", "use": "sig",
                "n": self.n, "e": self.e,
            })
        }

        pub(crate) fn sign(&self, claims: &serde_json::Value) -> String {
            let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
            header.kid = Some(self.kid.clone());
            jsonwebtoken::encode(&header, claims, &self.encoding).expect("sign token")
        }
    }

    /// A JWKS document in the shape Cloudflare serves (with its extra fields).
    pub(crate) fn jwks(keys: &[&TestKey]) -> String {
        serde_json::json!({
            "keys": keys.iter().map(|key| key.jwk()).collect::<Vec<_>>(),
            "public_cert": { "kid": "ignored", "cert": "-----BEGIN CERTIFICATE-----" },
            "public_certs": [],
        })
        .to_string()
    }

    pub(crate) fn now() -> u64 {
        jsonwebtoken::get_current_timestamp()
    }

    /// Claims of a person Access admitted.
    pub(crate) fn person_claims(email: &str) -> serde_json::Value {
        let now = now();
        serde_json::json!({
            "aud": [AUD], "email": email, "exp": now + 600, "iat": now, "nbf": now - 5,
            "iss": format!("https://{TEAM}"), "type": "app", "identity_nonce": "nonce",
            "sub": "7335d417-61da-459d-899c-0a01c76a2f94", "country": "UY",
        })
    }

    /// Claims of a Cloudflare service token.
    pub(crate) fn service_claims(client_id: &str) -> serde_json::Value {
        let now = now();
        serde_json::json!({
            "aud": [AUD], "common_name": client_id, "exp": now + 600, "iat": now,
            "nbf": now - 5, "iss": format!("https://{TEAM}"), "type": "app", "sub": "",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn access(admins: Option<&str>, key: &TestKey) -> CfAccess {
        let access = CfAccess::new(TEAM, AUD, admins).unwrap();
        access.install_keys(jwks(&[key]).as_bytes()).unwrap();
        access
    }

    #[test]
    fn access_token_signed_by_the_team_key_verifies_with_its_claims_checked() {
        let key = TestKey::generate("k1");
        let other = TestKey::generate("k1");
        let access = access(None, &key);

        let grant = access
            .verify(&key.sign(&person_claims("Vera@Example.com")))
            .unwrap();
        assert_eq!(
            grant.identity,
            AccessIdentity::Person {
                email: "vera@example.com".into()
            }
        );
        assert_eq!(grant.grant_profile, "session-all");
        assert!(grant.scopes().iter().any(|scope| scope == "rdf.update"));

        let mutate = |field: &str, value: serde_json::Value| {
            let mut claims = person_claims("vera@example.com");
            claims[field] = value;
            key.sign(&claims)
        };
        let now = now();
        for (why, token) in [
            (
                "wrong audience",
                mutate("aud", serde_json::json!(["some-other-app"])),
            ),
            (
                "wrong issuer",
                mutate(
                    "iss",
                    serde_json::json!("https://evil.cloudflareaccess.com"),
                ),
            ),
            ("expired", mutate("exp", serde_json::json!(now - 3600))),
            (
                "not yet valid",
                mutate("nbf", serde_json::json!(now + 3600)),
            ),
            ("no expiry", {
                let mut claims = person_claims("vera@example.com");
                claims.as_object_mut().unwrap().remove("exp");
                key.sign(&claims)
            }),
            (
                "signed by another key under the same kid",
                other.sign(&person_claims("vera@example.com")),
            ),
        ] {
            assert!(access.verify(&token).is_err(), "{why} must not verify");
        }

        // A tampered payload breaks the signature.
        let token = key.sign(&person_claims("vera@example.com"));
        let mut parts = token.split('.').map(str::to_string).collect::<Vec<_>>();
        let mut claims = person_claims("admin@example.com");
        claims["sub"] = serde_json::json!("someone-else");
        parts[1] = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            serde_json::to_vec(&claims).unwrap(),
        );
        assert!(access.verify(&parts.join(".")).is_err(), "tampered payload");

        // An unknown kid does not verify until the keys are refreshed.
        let rotated = TestKey::generate("k2");
        assert!(access
            .verify(&rotated.sign(&person_claims("vera@example.com")))
            .is_err());

        // alg confusion: an HS256 token "signed" with the public modulus.
        let mut header = jsonwebtoken::Header::new(Algorithm::HS256);
        header.kid = Some("k1".into());
        let hs = jsonwebtoken::encode(
            &header,
            &person_claims("vera@example.com"),
            &jsonwebtoken::EncodingKey::from_secret(key.jwk()["n"].as_str().unwrap().as_bytes()),
        )
        .unwrap();
        assert!(access.verify(&hs).is_err(), "HS256 must be refused");
    }

    #[test]
    fn admin_emails_split_session_all_from_members() {
        let key = TestKey::generate("k1");
        let open = access(None, &key);
        let listed = access(Some(" Vera@Example.com , lucy@example.com"), &key);

        let vera = key.sign(&person_claims("vera@example.com"));
        let cousin = key.sign(&person_claims("cousin@example.com"));
        let agent = key.sign(&service_claims("6fe0d2a1.access"));

        assert_eq!(open.verify(&vera).unwrap().grant_profile, ADMIN_GRANT);
        assert_eq!(open.verify(&cousin).unwrap().grant_profile, ADMIN_GRANT);
        assert_eq!(open.verify(&agent).unwrap().grant_profile, ADMIN_GRANT);

        assert_eq!(listed.verify(&vera).unwrap().grant_profile, ADMIN_GRANT);
        assert_eq!(listed.verify(&cousin).unwrap().grant_profile, MEMBER_GRANT);
        let service = listed.verify(&agent).unwrap();
        assert_eq!(
            service.identity,
            AccessIdentity::Service {
                client_id: "6fe0d2a1.access".into()
            }
        );
        assert_eq!(service.grant_profile, MEMBER_GRANT);
        assert_eq!(service.email(), None);

        let admin = listed.verify(&vera).unwrap().scopes();
        let member = listed.verify(&cousin).unwrap().scopes();
        let has = |scopes: &[String], scope: &str| scopes.iter().any(|held| held == scope);
        for scope in [
            "documents.write.crdt",
            "workspace.write.crdt",
            "artifacts.write",
            "mcp.tools.read",
            "mcp.tools.call",
            "rdf.query",
            "time-travel.read",
            "search.semantic.read",
            "loopback.manifest.read",
        ] {
            assert!(has(&member, scope), "a member holds {scope}");
        }
        for scope in [
            "graphs.write",
            "graphs.delete",
            "rdf.load",
            "rdf.update",
            "documents.delete.crdt",
            "workspace.delete.crdt",
            "time-travel.restore",
            "services.manage",
            "loopback.tokens.write",
        ] {
            assert!(!has(&member, scope), "a member lacks {scope}");
            assert!(has(&admin, scope), "an admin holds {scope}");
        }
        assert!(member.iter().all(|scope| has(&admin, scope)));

        // One definition: an Access member holds exactly what a
        // `gardend token create --grant member` token holds.
        let minted = session_all_token_grant()
            .grant_profiles
            .into_iter()
            .find(|profile| profile.id == MEMBER_GRANT_PROFILE)
            .expect("the member grant profile")
            .scopes;
        assert_eq!(
            member,
            minted
                .iter()
                .map(|scope| scope.to_string())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn unknown_kid_refetches_the_team_keys_over_http() {
        use axum::{routing::get, Router};
        let first = TestKey::generate("k1");
        let second = TestKey::generate("k2");
        let served = Arc::new(std::sync::Mutex::new(jwks(&[&first])));
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let app = Router::new().route(
            "/cdn-cgi/access/certs",
            get({
                let served = served.clone();
                let fetches = fetches.clone();
                move || async move {
                    fetches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    served.lock().unwrap().clone()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut access = CfAccess::new(TEAM, AUD, None).unwrap();
        access.certs_url = format!("http://{addr}/cdn-cgi/access/certs");
        access.refetch_interval = Duration::ZERO;

        let token = first.sign(&person_claims("vera@example.com"));
        assert!(access.verify(&token).is_err(), "no keys cached yet");
        access.ensure_signing_key(&token).await;
        assert!(access.verify(&token).is_ok());
        access.ensure_signing_key(&token).await;
        assert_eq!(
            fetches.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a known kid never refetches"
        );

        // Cloudflare rotates: the new kid triggers one refetch, the old key is gone.
        *served.lock().unwrap() = jwks(&[&second]);
        let rotated = second.sign(&person_claims("vera@example.com"));
        access.ensure_signing_key(&rotated).await;
        assert!(access.verify(&rotated).is_ok());
        assert!(
            access.verify(&token).is_err(),
            "rotated-out key no longer verifies"
        );
        assert_eq!(fetches.load(std::sync::atomic::Ordering::SeqCst), 2);

        // The refetch interval holds back a stream of unknown kids.
        access.refetch_interval = Duration::from_secs(3600);
        let forged = TestKey::generate("k3").sign(&person_claims("vera@example.com"));
        access.ensure_signing_key(&forged).await;
        access.ensure_signing_key(&forged).await;
        assert_eq!(fetches.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    fn settings() -> SelfHostSettings {
        SelfHostSettings::default()
    }

    #[test]
    fn no_knobs_is_off_and_lists_nothing() {
        let self_host = SelfHost::from_settings(SelfHostSettings {
            // Hosted cells set this; on its own it is not a self-host knob.
            public_base_url: Some("https://api.garden.sophia-labs.com/o/x/g/y".into()),
            binary_dir: Some(std::env::temp_dir().join("no-ui-here-selfhost")),
            ..settings()
        })
        .unwrap();
        assert!(!self_host.is_active());
        assert!(self_host.access().is_none());
        assert!(self_host.ui_dir().is_none());
        assert!(self_host.allowed_origins().is_empty());
        assert!(!self_host.origin_listed("https://api.garden.sophia-labs.com"));
        assert_eq!(self_host.auth_mode(), "key");
    }

    #[test]
    fn allowed_origins_are_normalised_and_default_to_the_public_base_under_access() {
        let listed = SelfHost::from_settings(SelfHostSettings {
            allowed_origins: Some("https://Sophia.Example.com/, http://box.local:8080 ,".into()),
            ..settings()
        })
        .unwrap();
        assert!(listed.is_active());
        assert_eq!(
            listed.allowed_origins(),
            [
                "http://box.local:8080".to_string(),
                "https://sophia.example.com".to_string()
            ]
        );
        assert!(listed.origin_listed("https://sophia.example.com"));
        assert!(listed.origin_listed("https://sophia.example.com:443"));
        assert!(!listed.origin_listed("http://sophia.example.com"));
        assert!(!listed.origin_listed("https://evil.example"));
        assert!(!listed.origin_listed("null"));
        assert!(SelfHost::from_settings(SelfHostSettings {
            allowed_origins: Some("not a url".into()),
            ..settings()
        })
        .is_err());

        let access = SelfHost::from_settings(SelfHostSettings {
            cf_access_team_domain: Some(format!("https://{TEAM}/")),
            cf_access_aud: Some(AUD.into()),
            public_base_url: Some("https://sophia.example.com/".into()),
            ..settings()
        })
        .unwrap();
        assert_eq!(access.auth_mode(), "cloudflare-access");
        assert_eq!(access.access().unwrap().team_domain(), TEAM);
        assert_eq!(
            access.allowed_origins(),
            ["https://sophia.example.com".to_string()]
        );

        assert!(
            SelfHost::from_settings(SelfHostSettings {
                cf_access_team_domain: Some(TEAM.into()),
                ..settings()
            })
            .is_err(),
            "team domain without an AUD is a misconfiguration"
        );
        assert!(
            SelfHost::from_settings(SelfHostSettings {
                cf_access_admin_emails: Some("vera@example.com".into()),
                ..settings()
            })
            .is_err(),
            "admin emails without Access is a misconfiguration"
        );
    }

    #[test]
    fn ambient_origin_rule_refuses_null_cross_site_and_unlisted_origins() {
        let self_host = SelfHost::from_settings(SelfHostSettings {
            allowed_origins: Some("https://sophia.example.com".into()),
            ..settings()
        })
        .unwrap();
        let headers = |pairs: &[(&'static str, &str)]| {
            let mut headers = HeaderMap::new();
            for (name, value) in pairs {
                headers.insert(*name, value.parse().unwrap());
            }
            headers
        };
        assert!(self_host.ambient_origin_ok(&headers(&[])));
        assert!(self_host.ambient_origin_ok(&headers(&[("origin", "https://sophia.example.com")])));
        assert!(self_host.ambient_origin_ok(&headers(&[("origin", "http://localhost:5173")])));
        assert!(!self_host.ambient_origin_ok(&headers(&[("origin", "null")])));
        assert!(!self_host.ambient_origin_ok(&headers(&[("origin", "https://evil.example")])));
        assert!(!self_host.ambient_origin_ok(&headers(&[("sec-fetch-site", "cross-site")])));
        assert!(self_host.ambient_origin_ok(&headers(&[("sec-fetch-site", "same-origin")])));
    }

    #[test]
    fn ui_dir_comes_from_the_knob_or_a_ui_folder_next_to_the_binary() {
        let root =
            std::env::temp_dir().join(format!("garden-selfhost-ui-{}", uuid::Uuid::new_v4()));
        let explicit = root.join("dist");
        std::fs::create_dir_all(&explicit).unwrap();
        assert!(
            SelfHost::from_settings(SelfHostSettings {
                ui_dir: Some(explicit.display().to_string()),
                ..settings()
            })
            .is_err(),
            "an explicit GARDEN_UI_DIR without index.html is refused at boot"
        );
        std::fs::write(explicit.join("index.html"), "<!doctype html>").unwrap();
        let chosen = SelfHost::from_settings(SelfHostSettings {
            ui_dir: Some(explicit.display().to_string()),
            ..settings()
        })
        .unwrap();
        assert!(chosen.is_active());
        assert_eq!(chosen.ui_dir(), Some(explicit.as_path()));

        let bin = root.join("bin");
        std::fs::create_dir_all(bin.join(UI_DIR_NEXT_TO_BINARY)).unwrap();
        let without_index = SelfHost::from_settings(SelfHostSettings {
            binary_dir: Some(bin.clone()),
            ..settings()
        })
        .unwrap();
        assert!(
            !without_index.is_active(),
            "an empty ui/ folder is not a UI"
        );
        std::fs::write(
            bin.join(UI_DIR_NEXT_TO_BINARY).join("index.html"),
            "<!doctype html>",
        )
        .unwrap();
        let next_to_binary = SelfHost::from_settings(SelfHostSettings {
            binary_dir: Some(bin.clone()),
            ..settings()
        })
        .unwrap();
        assert_eq!(
            next_to_binary.ui_dir(),
            Some(bin.join(UI_DIR_NEXT_TO_BINARY).as_path())
        );
        assert_eq!(next_to_binary.auth_mode(), "key");
        let _ = std::fs::remove_dir_all(&root);
    }
}

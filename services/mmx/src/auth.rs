//! Who may use `/admin/v1`, and what they may do. The CLI sends `MMX_ADMIN_TOKEN` as a bearer, and
//! the console holds a signed session cookie. An OIDC provider is the normal way in, and its groups
//! decide a person's role. The password is for first install and break-glass, and carries the admin
//! role, like the token.
//!
//! There's no default password. The first start reads `MMX_ADMIN_PASSWORD` or generates one and logs
//! it once, and only a hash is stored. That password only unlocks first-run setup, where the admin
//! chooses a real one.
//!
//! Destructive routes need a recent sign-in. A password session proves it by typing the password
//! again, and an OIDC session goes back to the provider, which must say when the person signed in.
//! The bearer token is exempt: it's a credential only a cluster admin can read, it can't be phished
//! through a browser, and there's nobody at a prompt in a script.
//!
//! A session is a signed cookie, so nothing about it is stored, and every replica can check it. A
//! generation number in the Secret revokes them all at once. Logging out only clears the cookie in
//! that browser.

use crate::oidc;
use crate::server::{ApiError, AppState, error, invalid, store_failed};
use crate::store::{ConfigStore, Data, SETTINGS_KEY, StoreError};

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{Extensions, HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use password_hash::SaltString;
use password_hash::rand_core::OsRng;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;
use tracing::{info, warn};

pub const SESSION_COOKIE: &str = "mmx_session";
const OIDC_COOKIE: &str = "mmx_oidc";
const OIDC_CALLBACK: &str = "/admin/v1/oidc/callback";
pub const CSRF_HEADER: &str = "x-csrf-token";

const SESSION_SECS: u64 = 12 * 60 * 60;
const OIDC_LOGIN_SECS: u64 = 10 * 60;
/// How long a sign-in counts as recent, for destructive actions.
pub const STEP_UP_SECS: u64 = 5 * 60;
/// Clocks at the provider and here can disagree by this much.
const CLOCK_SKEW_SECS: u64 = 60;
const MIN_PASSWORD: usize = 12;
const MAX_PASSWORD: usize = 1024;

const PASSWORD_HASH_KEY: &str = "password_hash";
const MUST_SET_KEY: &str = "password_must_change";
const SESSION_KEY_KEY: &str = "session_key";
const GENERATION_KEY: &str = "session_generation";
pub const OIDC_SECRET_KEY: &str = "oidc_client_secret";

/// Argon2 takes real memory, so a burst of logins has to queue.
const CONCURRENT_HASHES: usize = 4;
const FAILURE_WINDOW: Duration = Duration::from_secs(300);
const FAILURES_ALLOWED: u32 = 10;
const TRACKED_ADDRESSES: usize = 10_000;
/// Other replicas may change the password or settings. This is how late they find out.
const REFRESH_EVERY: Duration = Duration::from_secs(5);

fn random_text(bytes: usize) -> Result<String, String> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).map_err(|err| format!("no system randomness: {err}"))?;
    Ok(URL_SAFE_NO_PAD.encode(buffer))
}

/// A secret generated for an operator: 32 random bytes as hex, which is easy to paste into a config.
pub fn random_hex() -> Result<String, String> {
    let mut buffer = [0u8; 32];
    getrandom::fill(&mut buffer).map_err(|err| format!("no system randomness: {err}"))?;
    Ok(buffer.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Hashes first, so that neither the length nor the content can leak through timing.
pub fn same(left: &str, right: &str) -> bool {
    let left = Sha256::digest(left.as_bytes());
    let right = Sha256::digest(right.as_bytes());
    bool::from(left.ct_eq(&right))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| format!("can't hash the password: {err}"))
}

fn password_matches(password: &str, stored: &str) -> bool {
    PasswordHash::new(stored).is_ok_and(|hash| {
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
    })
}

fn check_new_password(password: &str) -> Result<(), ApiError> {
    let length = password.chars().count();
    if length < MIN_PASSWORD {
        return Err(invalid(format!("Use at least {MIN_PASSWORD} characters.")));
    }
    if length > MAX_PASSWORD {
        return Err(invalid(format!("Use at most {MAX_PASSWORD} characters.")));
    }
    Ok(())
}

/// What someone may do. Each role includes the ones before it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Operator,
    Admin,
}

/// Provider groups for each role, by exact name. Someone in several gets the highest.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct RoleGroups {
    #[serde(default)]
    pub admin: Vec<String>,
    #[serde(default)]
    pub operator: Vec<String>,
    #[serde(default)]
    pub viewer: Vec<String>,
}

impl RoleGroups {
    /// Nobody gets in by default: a group that isn't listed is worth nothing.
    pub fn role_for(&self, groups: &[String]) -> Option<Role> {
        let holds = |listed: &[String]| groups.iter().any(|group| listed.contains(group));
        if holds(&self.admin) {
            Some(Role::Admin)
        } else if holds(&self.operator) {
            Some(Role::Operator)
        } else if holds(&self.viewer) {
            Some(Role::Viewer)
        } else {
            None
        }
    }
}

pub const DEFAULT_GROUPS_CLAIM: &str = "groups";

fn default_groups_claim() -> String {
    DEFAULT_GROUPS_CLAIM.to_string()
}

/// Older files can hold fields that are gone, such as the sign-in list, so unknown fields are
/// ignored here. They're dropped on the next save.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct OidcSettings {
    pub enabled: bool,
    pub issuer: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default = "default_groups_claim")]
    pub groups_claim: String,
    #[serde(default)]
    pub roles: RoleGroups,
}

/// `console.yaml` in the store. None of it is secret.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Settings {
    /// Left out, it's whatever the deployment asks for, and on by default. A stored value wins, so
    /// `mmx auth password-login on` works as a way back in on any install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_login: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc: Option<OidcSettings>,
}

/// What the deployment sets in the environment, so an install can start with its provider already
/// connected. A provider set here can't be changed from the console.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Managed {
    pub oidc: Option<OidcSettings>,
    pub client_secret: Option<String>,
    pub password_login: Option<bool>,
}

fn group_list(text: Option<String>) -> Vec<String> {
    text.unwrap_or_default()
        .split(',')
        .map(|group| group.trim().to_string())
        .filter(|group| !group.is_empty())
        .collect()
}

impl Managed {
    /// `read` is the environment. Half a provider stops the server here, rather than leaving
    /// everyone locked out with no explanation.
    pub fn read(read: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let text = |name: &str| read(name).filter(|value| !value.trim().is_empty());
        let password_login = match text("MMX_PASSWORD_LOGIN").as_deref() {
            None => None,
            Some("true") => Some(true),
            Some("false") => Some(false),
            Some(other) => {
                return Err(format!(
                    "MMX_PASSWORD_LOGIN must be true or false, not {other}"
                ));
            }
        };
        let Some(issuer) = text("MMX_OIDC_ISSUER") else {
            return Ok(Self {
                password_login,
                ..Self::default()
            });
        };
        oidc::check_url("MMX_OIDC_ISSUER", issuer.trim())?;
        let client_id = text("MMX_OIDC_CLIENT_ID")
            .ok_or("MMX_OIDC_ISSUER is set, but MMX_OIDC_CLIENT_ID isn't")?;
        let client_secret = text("MMX_OIDC_CLIENT_SECRET")
            .ok_or("MMX_OIDC_ISSUER is set, but MMX_OIDC_CLIENT_SECRET isn't")?;
        let roles = RoleGroups {
            admin: group_list(text("MMX_OIDC_ADMIN_GROUPS")),
            operator: group_list(text("MMX_OIDC_OPERATOR_GROUPS")),
            viewer: group_list(text("MMX_OIDC_VIEWER_GROUPS")),
        };
        if roles.admin.is_empty() {
            return Err(
                "MMX_OIDC_ADMIN_GROUPS is empty, so nobody could run this install".to_string(),
            );
        }
        Ok(Self {
            oidc: Some(OidcSettings {
                enabled: true,
                issuer: issuer.trim().trim_end_matches('/').to_string(),
                client_id: client_id.trim().to_string(),
                display_name: text("MMX_OIDC_DISPLAY_NAME"),
                groups_claim: text("MMX_OIDC_GROUPS_CLAIM").unwrap_or_else(default_groups_claim),
                roles,
            }),
            client_secret: Some(client_secret),
            password_login,
        })
    }
}

impl Settings {
    fn read(data: &Data) -> Result<Self, String> {
        match data.get(SETTINGS_KEY) {
            Some(text) => {
                serde_yaml::from_str(text).map_err(|err| format!("{SETTINGS_KEY}: {err}"))
            }
            None => Ok(Self::default()),
        }
    }

    pub fn oidc_enabled(&self) -> Option<&OidcSettings> {
        self.oidc.as_ref().filter(|oidc| oidc.enabled)
    }
}

/// Reads settings, applies a change and saves them.
pub async fn update_settings<F>(store: &ConfigStore, mut change: F) -> Result<Settings, StoreError>
where
    F: FnMut(&mut Settings) -> Result<(), StoreError>,
{
    let doc = store
        .update_config(|data| {
            let mut settings = Settings::read(data).map_err(StoreError::Failed)?;
            change(&mut settings)?;
            let text = serde_yaml::to_string(&settings)
                .map_err(|err| StoreError::Failed(err.to_string()))?;
            data.insert(SETTINGS_KEY.to_string(), text);
            Ok(())
        })
        .await?;
    Settings::read(&doc.data).map_err(StoreError::Failed)
}

/// What every request checks against, read from the store and held in memory, because a session
/// check can't be a Kubernetes API call.
#[derive(Clone, Default)]
pub struct Keys {
    session_key: Vec<u8>,
    generation: u64,
    password_hash: Option<String>,
    must_set: bool,
    pub settings: Settings,
    pub password_login: bool,
    oidc_secret: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub user: String,
    pub method: String,
    pub role: Role,
    /// When this person last proved who they are. Zero if a provider never said.
    auth_at: u64,
    expires: u64,
    generation: u64,
    csrf: String,
}

/// Who a request came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    Token,
    /// `--insecure-local`, from something other than a browser.
    Local,
    Session(Session),
}

impl Caller {
    fn role(&self) -> Role {
        match self {
            Self::Token | Self::Local => Role::Admin,
            Self::Session(session) => session.role,
        }
    }

    /// Who to name in the audit log.
    pub fn name(&self) -> &str {
        match self {
            Self::Token => "the admin token",
            Self::Local => "a local caller",
            Self::Session(session) => &session.user,
        }
    }
}

pub struct Auth {
    store: Arc<ConfigStore>,
    /// Hashed, so that comparing doesn't reveal the token's length.
    token: Option<[u8; 32]>,
    insecure_local: bool,
    managed: Managed,
    keys: RwLock<Arc<Keys>>,
    hashing: Semaphore,
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
    oidc_http: reqwest::Client,
}

pub fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn sign(key: &[u8], payload: &str) -> Result<String, String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|err| err.to_string())?;
    mac.update(payload.as_bytes());
    Ok(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

fn seal<T: Serialize>(key: &[u8], value: &T) -> Result<String, String> {
    let json = serde_json::to_vec(value).map_err(|err| err.to_string())?;
    let payload = URL_SAFE_NO_PAD.encode(json);
    let signature = sign(key, &payload)?;
    Ok(format!("{payload}.{signature}"))
}

fn open<T: for<'de> Deserialize<'de>>(key: &[u8], sealed: &str) -> Option<T> {
    let (payload, signature) = sealed.split_once('.')?;
    let sent = URL_SAFE_NO_PAD.decode(signature).ok()?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).ok()?;
    mac.update(payload.as_bytes());
    mac.verify_slice(&sent).ok()?;
    let json = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&json).ok()
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then_some(value)
        })
}

/// One value from a header a proxy sets. The first is the one the client reached.
fn forwarded<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let value = headers.get(name)?.to_str().ok()?;
    let first = value.split(',').next()?.trim();
    (!first.is_empty()).then_some(first)
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-_:[]".contains(&byte))
}

/// The scheme and host this request was addressed to. These headers can be forged, which is safe
/// for what they're used for. A forged host only changes the redirect URI, which the provider
/// compares with the one registered, and a forged scheme only hides a cookie from its own sender.
/// Nothing else trusts them, and they never decide who someone is.
pub fn request_origin(headers: &HeaderMap) -> Option<String> {
    let host = forwarded(headers, "x-forwarded-host")
        .filter(|host| valid_host(host))
        .or_else(|| {
            headers
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
                .filter(|host| valid_host(host))
        })?;
    let scheme = if secure(headers) { "https" } else { "http" };
    Some(format!("{scheme}://{host}"))
}

/// Exactly what to register with a provider.
pub fn redirect_uri(headers: &HeaderMap) -> Option<String> {
    Some(format!("{}{OIDC_CALLBACK}", request_origin(headers)?))
}

/// Secure cookies need TLS, which is terminated in front of this server, so only the proxy knows.
fn secure(headers: &HeaderMap) -> bool {
    forwarded(headers, "x-forwarded-proto").is_some_and(|proto| proto.eq_ignore_ascii_case("https"))
}

fn cookie(
    name: &str,
    value: &str,
    age: u64,
    same_site: &str,
    secure: bool,
) -> Result<HeaderValue, ApiError> {
    let flag = if secure { "; Secure" } else { "" };
    let text =
        format!("{name}={value}; Path=/; HttpOnly; SameSite={same_site}; Max-Age={age}{flag}");
    HeaderValue::from_str(&text).map_err(|err| internal(err.to_string()))
}

fn internal(message: String) -> ApiError {
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
}

/// A browser always names the page a write came from. This backs up the CSRF token, and covers
/// login, which has no session to hold one yet. A page on another site can't set `X-Forwarded-Host`
/// without a CORS preflight, which this server never answers, so accepting it is safe, and needed
/// behind a proxy that rewrites `Host`.
fn check_origin(headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let origin = origin.to_str().unwrap_or_default();
    let host = origin.split_once("://").map_or(origin, |(_, host)| host);
    let same =
        |expected: Option<&str>| expected.is_some_and(|name| name.eq_ignore_ascii_case(host));
    let direct = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    if same(direct) || same(forwarded(headers, "x-forwarded-host")) {
        Ok(())
    } else {
        Err(error(
            StatusCode::FORBIDDEN,
            "bad_origin",
            "This request came from another site.",
        ))
    }
}

impl Auth {
    /// Creates what a first start needs, then loads it. Replicas can start together, because writes
    /// are conditional, so only the one that won logs a password.
    pub async fn start(
        store: Arc<ConfigStore>,
        token: Option<&str>,
        insecure_local: bool,
        initial_password: Option<String>,
        managed: Managed,
    ) -> Result<Arc<Self>, String> {
        let auth = Arc::new(Self {
            store,
            token: token.map(digest),
            insecure_local,
            managed,
            keys: RwLock::new(Arc::new(Keys::default())),
            hashing: Semaphore::new(CONCURRENT_HASHES),
            failures: Mutex::default(),
            oidc_http: oidc::client()?,
        });
        auth.bootstrap(initial_password).await?;
        auth.refresh().await?;
        Ok(auth)
    }

    async fn bootstrap(&self, initial_password: Option<String>) -> Result<(), String> {
        let doc = self.store.secrets().await.map_err(|err| err.message())?;
        if doc.data.contains_key(PASSWORD_HASH_KEY) && doc.data.contains_key(SESSION_KEY_KEY) {
            return Ok(());
        }

        let generated = initial_password.is_none();
        let password = match initial_password {
            Some(password) => password,
            None => random_text(18)?,
        };
        let hash = hash_password(&password)?;
        let session_key = random_text(32)?;

        let mut doc = doc;
        doc.data
            .entry(SESSION_KEY_KEY.to_string())
            .or_insert(session_key);
        if !doc.data.contains_key(PASSWORD_HASH_KEY) {
            doc.data.insert(PASSWORD_HASH_KEY.to_string(), hash);
            doc.data
                .insert(MUST_SET_KEY.to_string(), "true".to_string());
        }
        // With a provider and no password login, nobody will use this. `mmx auth reset-password`
        // makes a usable one for break-glass.
        let unused = self.managed.password_login == Some(false);
        match self.store.put_secrets(&doc).await {
            Ok(_) if unused => Ok(()),
            Ok(_) if generated => {
                info!("Console: first-run admin password: {password}");
                info!("Console: this is shown once. Sign in with it to choose a real password.");
                Ok(())
            }
            Ok(_) => {
                info!("Console: first-run admin password taken from MMX_ADMIN_PASSWORD");
                Ok(())
            }
            // Another replica got there first, and its write is as good as this one.
            Err(StoreError::Conflict) => Ok(()),
            Err(err) => Err(err.message()),
        }
    }

    pub async fn refresh(&self) -> Result<(), String> {
        let secrets = self.store.secrets().await.map_err(|err| err.message())?;
        let config = self.store.config().await.map_err(|err| err.message())?;
        let field = |name: &str| secrets.data.get(name).cloned();
        let mut settings = Settings::read(&config.data)?;
        if let Some(oidc) = &self.managed.oidc {
            settings.oidc = Some(oidc.clone());
        }
        let keys = Keys {
            session_key: field(SESSION_KEY_KEY)
                .map(String::into_bytes)
                .ok_or("the Secret has no session key")?,
            generation: field(GENERATION_KEY)
                .and_then(|text| text.parse().ok())
                .unwrap_or(0),
            password_hash: field(PASSWORD_HASH_KEY),
            must_set: field(MUST_SET_KEY).is_some_and(|text| text == "true"),
            password_login: settings
                .password_login
                .or(self.managed.password_login)
                .unwrap_or(true),
            settings,
            oidc_secret: self
                .managed
                .client_secret
                .clone()
                .or_else(|| field(OIDC_SECRET_KEY)),
        };
        *self.keys.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(keys);
        Ok(())
    }

    pub fn spawn_refresh(self: &Arc<Self>) {
        let auth = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(REFRESH_EVERY).await;
                if let Err(err) = auth.refresh().await {
                    warn!("Console: can't refresh settings: {err}");
                }
            }
        });
    }

    pub fn keys(&self) -> Arc<Keys> {
        self.keys
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// Whether the provider comes from the deployment, not from this console.
    pub fn oidc_managed(&self) -> bool {
        self.managed.oidc.is_some()
    }

    pub fn oidc_secret_hint(&self) -> Option<String> {
        self.keys().oidc_secret.as_deref().map(hint)
    }

    pub fn oidc_http(&self) -> &reqwest::Client {
        &self.oidc_http
    }

    fn session(&self, keys: &Keys, headers: &HeaderMap) -> Option<Session> {
        let session: Session = open(&keys.session_key, cookie_value(headers, SESSION_COOKIE)?)?;
        (session.expires > now() && session.generation == keys.generation).then_some(session)
    }

    fn issue(
        &self,
        keys: &Keys,
        user: &str,
        method: &str,
        role: Role,
        auth_at: u64,
    ) -> Result<(String, Session), String> {
        let session = Session {
            user: user.to_string(),
            method: method.to_string(),
            role,
            auth_at,
            expires: now() + SESSION_SECS,
            generation: keys.generation,
            csrf: random_text(24)?,
        };
        Ok((seal(&keys.session_key, &session)?, session))
    }

    /// The same session under the current generation, so the person keeps working.
    pub fn reissue(&self, session: &Session, headers: &HeaderMap) -> Result<HeaderValue, ApiError> {
        let keys = self.keys();
        let renewed = Session {
            generation: keys.generation,
            ..session.clone()
        };
        let sealed = seal(&keys.session_key, &renewed).map_err(internal)?;
        cookie(
            SESSION_COOKIE,
            &sealed,
            SESSION_SECS,
            "Strict",
            secure(headers),
        )
    }

    fn check_bearer(&self, presented: &str) -> bool {
        self.token
            .as_ref()
            .is_some_and(|expected| bool::from(digest(presented).ct_eq(expected)))
    }

    /// `setup_allowed` is for the few routes open to someone who hasn't chosen a password yet.
    fn identify(
        &self,
        method: &Method,
        headers: &HeaderMap,
        setup_allowed: bool,
    ) -> Result<Caller, ApiError> {
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        if let Some(token) = bearer {
            return if self.check_bearer(token) {
                Ok(Caller::Token)
            } else {
                Err(unauthenticated())
            };
        }

        let keys = self.keys();
        if let Some(session) = self.session(&keys, headers) {
            if !method.is_safe() {
                check_origin(headers)?;
                let csrf = headers
                    .get(CSRF_HEADER)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default();
                if !same(csrf, &session.csrf) {
                    return Err(error(
                        StatusCode::FORBIDDEN,
                        "bad_csrf_token",
                        "This page is out of date. Reload it and try again.",
                    ));
                }
            }
            if keys.must_set && session.method == "password" && !setup_allowed {
                return Err(error(
                    StatusCode::FORBIDDEN,
                    "password_setup_required",
                    "Choose a password first.",
                ));
            }
            return Ok(Caller::Session(session));
        }

        // A web page can post to localhost, and it can't be stopped from sending no credentials, so
        // the open door is only for callers that aren't browsers.
        if self.insecure_local && self.token.is_none() && !headers.contains_key(header::ORIGIN) {
            return Ok(Caller::Local);
        }
        Err(unauthenticated())
    }

    fn peek_blocked(&self, peer: Option<IpAddr>) -> bool {
        let Some(peer) = peer else { return false };
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        match failures.get(&peer) {
            Some((_, since)) if since.elapsed() > FAILURE_WINDOW => {
                failures.remove(&peer);
                false
            }
            Some((count, _)) => *count >= FAILURES_ALLOWED,
            None => false,
        }
    }

    fn note_login(&self, peer: Option<IpAddr>, succeeded: bool) {
        let Some(peer) = peer else { return };
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        if succeeded {
            failures.remove(&peer);
            return;
        }
        if failures.len() >= TRACKED_ADDRESSES {
            failures.clear();
        }
        failures.entry(peer).or_insert((0, Instant::now())).0 += 1;
    }

    /// Stores a new password and revokes every session. `must_set` means it's a temporary one.
    async fn set_password(&self, password: &str, must_set: bool) -> Result<(), ApiError> {
        let hash = {
            let _slot = self
                .hashing
                .acquire()
                .await
                .map_err(|err| internal(err.to_string()))?;
            let password = password.to_string();
            tokio::task::spawn_blocking(move || hash_password(&password))
                .await
                .map_err(|err| internal(err.to_string()))?
                .map_err(internal)?
        };
        self.store
            .update_secrets(|data| {
                data.insert(PASSWORD_HASH_KEY.to_string(), hash.clone());
                data.insert(MUST_SET_KEY.to_string(), must_set.to_string());
                bump_generation(data);
                Ok(())
            })
            .await
            .map_err(store_failed)?;
        self.refresh().await.map_err(internal)
    }

    pub async fn revoke_sessions(&self) -> Result<(), ApiError> {
        self.store
            .update_secrets(|data| {
                bump_generation(data);
                Ok(())
            })
            .await
            .map_err(store_failed)?;
        self.refresh().await.map_err(internal)
    }

    pub async fn set_oidc_secret(&self, secret: &str) -> Result<(), ApiError> {
        self.store
            .update_secrets(|data| {
                data.insert(OIDC_SECRET_KEY.to_string(), secret.to_string());
                Ok(())
            })
            .await
            .map_err(store_failed)?;
        self.refresh().await.map_err(internal)
    }

    async fn verify_password(&self, password: String, stored: String) -> Result<bool, ApiError> {
        let _slot = self
            .hashing
            .acquire()
            .await
            .map_err(|err| internal(err.to_string()))?;
        tokio::task::spawn_blocking(move || password_matches(&password, &stored))
            .await
            .map_err(|err| internal(err.to_string()))
    }
}

fn bump_generation(data: &mut Data) {
    let next = data
        .get(GENERATION_KEY)
        .and_then(|text| text.parse::<u64>().ok())
        .unwrap_or(0)
        + 1;
    data.insert(GENERATION_KEY.to_string(), next.to_string());
}

/// The last few characters, so an operator can tell which secret is set.
pub fn hint(secret: &str) -> String {
    let tail: String = secret
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("...{tail}")
}

fn unauthenticated() -> ApiError {
    error(
        StatusCode::UNAUTHORIZED,
        "unauthenticated",
        "Sign in, or send a valid bearer token.",
    )
}

fn refuse(err: ApiError) -> Response {
    let mut response = err.into_response();
    if response.status() == StatusCode::UNAUTHORIZED {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    response
}

/// The role a route needs. Reading is open to any role, apart from settings, and anything not
/// named here takes admin, so a new route is safe until someone decides otherwise.
pub fn required_role(method: &Method, path: &str) -> Role {
    let path = path.strip_prefix("/admin/v1").unwrap_or(path);
    if method.is_safe() {
        return if path == "/settings" {
            Role::Admin
        } else {
            Role::Viewer
        };
    }
    if path.starts_with("/markets/")
        || path == "/config/markets"
        || path == "/config/markets/validate"
    {
        return Role::Operator;
    }
    Role::Admin
}

/// Routes that can't be undone, or that decide who gets in. Saving the markets file can delete a
/// market too, so that route checks for itself.
pub fn needs_step_up(method: &Method, path: &str) -> bool {
    let path = path.strip_prefix("/admin/v1").unwrap_or(path);
    let market = path.strip_prefix("/markets/");
    match *method {
        Method::DELETE => market.is_some(),
        Method::POST => {
            market.is_some_and(|rest| rest.ends_with("/cancel-all"))
                || path == "/webhooks/secret"
                || path == "/sessions/revoke"
        }
        Method::PUT => path == "/settings" || path == "/settings/oidc",
        _ => false,
    }
}

/// A session must have signed in recently. The token and local callers pass, as the module says.
pub fn check_recent(caller: &Caller) -> Result<(), ApiError> {
    let Caller::Session(session) = caller else {
        return Ok(());
    };
    if now().saturating_sub(session.auth_at) <= STEP_UP_SECS {
        return Ok(());
    }
    Err(error(
        StatusCode::FORBIDDEN,
        "reauth_required",
        "Confirm who you are to do this.",
    ))
}

/// Everything under `/admin/v1`, apart from signing in. Every write is logged with who made it.
pub async fn require_role(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let caller = match state.auth.identify(&method, request.headers(), false) {
        Ok(caller) => caller,
        Err(err) => return refuse(err),
    };
    if caller.role() < required_role(&method, &path) {
        warn!("Audit: refused {} {method} {path}", caller.name());
        return refuse(error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Your role doesn't allow this.",
        ));
    }
    if needs_step_up(&method, &path)
        && let Err(err) = check_recent(&caller)
    {
        return refuse(err);
    }
    let name = (!method.is_safe()).then(|| caller.name().to_string());
    request.extensions_mut().insert(caller);
    let response = next.run(request).await;
    if let Some(name) = name {
        info!(
            "Audit: {name} {method} {path}: {}",
            response.status().as_u16()
        );
    }
    response
}

/// The routes someone can use with only the temporary password.
pub async fn require_signed_in(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    match state
        .auth
        .identify(request.method(), request.headers(), true)
    {
        Ok(caller) => {
            request.extensions_mut().insert(caller);
            next.run(request).await
        }
        Err(err) => refuse(err),
    }
}

fn session_view(state: &AppState, keys: &Keys, session: Option<&Session>) -> Value {
    let oidc = keys.settings.oidc_enabled().map(|oidc| {
        json!({ "label": oidc.display_name.clone().unwrap_or_else(|| "single sign-on".to_string()) })
    });
    json!({
        "authenticated": session.is_some(),
        "user": session.map(|session| &session.user),
        "method": session.map(|session| &session.method),
        "csrf_token": session.map(|session| &session.csrf),
        "role": session.map(|session| session.role),
        "reauth_until": session.map(|session| session.auth_at + STEP_UP_SECS),
        "must_set_password": session.is_some_and(|session| session.method == "password") && keys.must_set,
        "first_run": keys.must_set && keys.password_login,
        "password_login": keys.password_login,
        "oidc": oidc,
        "version": env!("CARGO_PKG_VERSION"),
        "store": state.store.info().backend,
    })
}

pub async fn current_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Json<Value> {
    let keys = state.auth.keys();
    let session = state.auth.session(&keys, &headers);
    Json(session_view(&state, &keys, session.as_ref()))
}

fn with_session(
    state: &AppState,
    keys: &Keys,
    headers: &HeaderMap,
    sealed: &str,
    session: &Session,
) -> Result<Response, ApiError> {
    let value = cookie(
        SESSION_COOKIE,
        sealed,
        SESSION_SECS,
        "Strict",
        secure(headers),
    )?;
    let mut response = Json(session_view(state, keys, Some(session))).into_response();
    response.headers_mut().insert(header::SET_COOKIE, value);
    Ok(response)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginBody {
    password: String,
}

fn peer_of(extensions: &Extensions) -> Option<IpAddr> {
    extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(address)| address.ip())
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    extensions: Extensions,
    headers: HeaderMap,
    body: Result<Json<LoginBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = body.map_err(|rejection| invalid(rejection.body_text()))?;
    let keys = state.auth.keys();
    check_origin(&headers)?;
    if !keys.password_login {
        return Err(error(
            StatusCode::FORBIDDEN,
            "password_login_disabled",
            "Password login is turned off.",
        ));
    }

    let peer = peer_of(&extensions);
    if state.auth.peek_blocked(peer) {
        return Err(error(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_attempts",
            "Too many failed attempts. Try again in a few minutes.",
        ));
    }

    let stored = keys.password_hash.clone().unwrap_or_default();
    let accepted = state.auth.verify_password(body.password, stored).await?;
    state.auth.note_login(peer, accepted);
    if !accepted {
        warn!("Console: failed password login from {peer:?}");
        return Err(error(
            StatusCode::UNAUTHORIZED,
            "wrong_password",
            "That password isn't right.",
        ));
    }
    let (sealed, session) = state
        .auth
        .issue(&keys, "admin", "password", Role::Admin, now())
        .map_err(internal)?;
    with_session(&state, &keys, &headers, &sealed, &session)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReauthBody {
    password: String,
}

/// Proves a password session is still the person who opened it. An OIDC session uses
/// `/oidc/start?reauth=1` instead.
pub async fn reauth(
    State(state): State<Arc<AppState>>,
    axum::Extension(caller): axum::Extension<Caller>,
    extensions: Extensions,
    headers: HeaderMap,
    body: Result<Json<ReauthBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = body.map_err(|rejection| invalid(rejection.body_text()))?;
    let Caller::Session(session) = caller else {
        return Err(invalid("Only a console session confirms this way."));
    };
    if session.method != "password" {
        return Err(invalid("Confirm with your provider."));
    }
    let peer = peer_of(&extensions);
    if state.auth.peek_blocked(peer) {
        return Err(error(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_attempts",
            "Too many failed attempts. Try again in a few minutes.",
        ));
    }
    let keys = state.auth.keys();
    let stored = keys.password_hash.clone().unwrap_or_default();
    let accepted = state.auth.verify_password(body.password, stored).await?;
    state.auth.note_login(peer, accepted);
    if !accepted {
        warn!(
            "Console: failed confirmation for {} from {peer:?}",
            session.user
        );
        return Err(error(
            StatusCode::UNAUTHORIZED,
            "wrong_password",
            "That password isn't right.",
        ));
    }
    let (sealed, session) = state
        .auth
        .issue(&keys, &session.user, &session.method, session.role, now())
        .map_err(internal)?;
    with_session(&state, &keys, &headers, &sealed, &session)
}

pub async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let keys = state.auth.keys();
    let value = cookie(SESSION_COOKIE, "", 0, "Strict", secure(&headers))?;
    let mut response = Json(session_view(&state, &keys, None)).into_response();
    response.headers_mut().insert(header::SET_COOKIE, value);
    Ok(response)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordBody {
    #[serde(default)]
    current: Option<String>,
    new: String,
}

/// Sets the password and signs everyone else out. Someone who just used the temporary password
/// doesn't have to type it again, and neither does a provider's user, who has no password yet.
pub async fn change_password(
    State(state): State<Arc<AppState>>,
    axum::Extension(caller): axum::Extension<Caller>,
    headers: HeaderMap,
    body: Result<Json<PasswordBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = body.map_err(|rejection| invalid(rejection.body_text()))?;
    check_new_password(&body.new)?;
    let keys = state.auth.keys();
    let session = match &caller {
        Caller::Session(session) => session.clone(),
        _ => {
            return Err(invalid(
                "Only a console session sets a password this way. The CLI uses `mmx auth reset-password`.",
            ));
        }
    };
    if session.method != "password" {
        if session.role != Role::Admin {
            return Err(error(
                StatusCode::FORBIDDEN,
                "forbidden",
                "Your role doesn't allow this.",
            ));
        }
        check_recent(&caller)?;
    }
    if session.method == "password" && !keys.must_set {
        let current = body.current.clone().unwrap_or_default();
        let stored = keys.password_hash.clone().unwrap_or_default();
        if !state.auth.verify_password(current, stored).await? {
            return Err(error(
                StatusCode::UNAUTHORIZED,
                "wrong_password",
                "The current password isn't right.",
            ));
        }
    }
    if let Some(stored) = keys.password_hash.clone()
        && state.auth.verify_password(body.new.clone(), stored).await?
    {
        return Err(invalid(
            "Choose a password that's different from the old one.",
        ));
    }

    state.auth.set_password(&body.new, false).await?;
    info!("Console: the admin password changed");
    let keys = state.auth.keys();
    let (sealed, session) = state
        .auth
        .issue(&keys, &session.user, &session.method, session.role, now())
        .map_err(internal)?;
    with_session(&state, &keys, &headers, &sealed, &session)
}

/// For an admin who's locked out, with the token. The new password is temporary, like the first.
pub async fn reset_password(
    State(state): State<Arc<AppState>>,
    axum::Extension(caller): axum::Extension<Caller>,
) -> Result<Json<Value>, ApiError> {
    if matches!(caller, Caller::Session(_)) {
        return Err(error(
            StatusCode::FORBIDDEN,
            "token_required",
            "This takes the admin token.",
        ));
    }
    let password = random_text(18).map_err(internal)?;
    state.auth.set_password(&password, true).await?;
    warn!("Console: the admin password was reset with the admin token");
    Ok(Json(json!({ "password": password })))
}

pub async fn revoke_sessions(
    State(state): State<Arc<AppState>>,
    axum::Extension(caller): axum::Extension<Caller>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    state.auth.revoke_sessions().await?;
    info!("Console: every session was revoked");
    let keys = state.auth.keys();
    match caller {
        // The person asking stays signed in.
        Caller::Session(session) => {
            let (sealed, session) = state
                .auth
                .issue(
                    &keys,
                    &session.user,
                    &session.method,
                    session.role,
                    session.auth_at,
                )
                .map_err(internal)?;
            with_session(&state, &keys, &headers, &sealed, &session)
        }
        _ => Ok(Json(json!({ "revoked": true })).into_response()),
    }
}

/// What the browser carries while it's away at the provider. The redirect URI travels with it, so
/// the callback sends the provider exactly what the start did.
#[derive(Serialize, Deserialize, Debug)]
struct OidcLogin {
    state: String,
    nonce: String,
    verifier: String,
    expires: u64,
    redirect_uri: String,
    /// Where to land afterwards, always a path on this server.
    return_to: String,
    step_up: bool,
}

fn login_failed(message: &str) -> Response {
    warn!("Console: OIDC login failed: {message}");
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("error", message)
        .finish();
    Redirect::to(&format!("/login?{query}")).into_response()
}

/// A failed confirmation leaves the session as it was, so the person goes back to their page.
fn step_up_failed(return_to: &str, message: &str) -> Response {
    warn!("Console: OIDC confirmation failed: {message}");
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("reauth_error", message)
        .finish();
    let glue = if return_to.contains('?') { '&' } else { '?' };
    Redirect::to(&format!("{return_to}{glue}{query}")).into_response()
}

/// A path on this server, or the home page. `//host` and `/\host` are other sites to a browser.
fn local_path(path: Option<&str>) -> String {
    match path {
        Some(path)
            if path.starts_with('/')
                && !path.starts_with("//")
                && !path.contains('\\')
                && path.chars().all(|c| !c.is_control()) =>
        {
            path.to_string()
        }
        _ => "/".to_string(),
    }
}

#[derive(Deserialize, Debug, Default)]
pub struct StartParams {
    /// Sends the person back to the provider to sign in again, for a destructive action.
    #[serde(default)]
    reauth: Option<String>,
    #[serde(rename = "return", default)]
    return_to: Option<String>,
}

async fn start_oidc(
    state: &AppState,
    keys: &Keys,
    headers: &HeaderMap,
    params: &StartParams,
) -> Result<Response, String> {
    let settings = keys
        .settings
        .oidc_enabled()
        .ok_or("Single sign-on isn't turned on.")?;
    let redirect = redirect_uri(headers).ok_or("This request has no usable Host header.")?;
    let provider = oidc::discover(state.auth.oidc_http(), &settings.issuer).await?;

    let step_up = params.reauth.is_some();
    let login = OidcLogin {
        state: random_text(24)?,
        nonce: random_text(24)?,
        verifier: random_text(48)?,
        expires: now() + OIDC_LOGIN_SECS,
        redirect_uri: redirect,
        return_to: local_path(params.return_to.as_deref()),
        step_up,
    };
    let address = oidc::authorize_url(
        &provider,
        &oidc::Request {
            client_id: &settings.client_id,
            redirect_uri: &login.redirect_uri,
            state: &login.state,
            nonce: &login.nonce,
            verifier: &login.verifier,
            max_age: step_up.then_some(STEP_UP_SECS),
        },
    )?;
    let sealed = seal(&keys.session_key, &login)?;
    // Lax, because the provider's redirect back is a navigation from another site.
    let value = cookie(
        OIDC_COOKIE,
        &sealed,
        OIDC_LOGIN_SECS,
        "Lax",
        secure(headers),
    )
    .map_err(|_| "can't build the login cookie".to_string())?;
    let mut response = Redirect::to(&address).into_response();
    response.headers_mut().insert(header::SET_COOKIE, value);
    Ok(response)
}

pub async fn oidc_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<StartParams>,
) -> Response {
    let keys = state.auth.keys();
    match start_oidc(&state, &keys, &headers, &params).await {
        Ok(response) => response,
        Err(message) if params.reauth.is_some() => {
            step_up_failed(&local_path(params.return_to.as_deref()), &message)
        }
        Err(message) => login_failed(&message),
    }
}

#[derive(Deserialize, Debug)]
pub struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Checks the provider's answer to a confirmation. A provider that ignores `prompt=login` still
/// reports when the person really signed in, so an old session can't pass for a new one.
fn check_step_up(auth_time: Option<u64>) -> Result<u64, String> {
    let at = auth_time.ok_or(
        "The provider didn't say when you signed in, so this can't be confirmed. \
         Turn on the auth_time claim for this client.",
    )?;
    if now().saturating_sub(at) > STEP_UP_SECS + CLOCK_SKEW_SECS {
        return Err(
            "The provider reused an earlier sign-in, so this wasn't confirmed.".to_string(),
        );
    }
    Ok(at)
}

async fn finish_oidc(
    state: &AppState,
    keys: &Keys,
    headers: &HeaderMap,
    login: &OidcLogin,
    params: CallbackParams,
) -> Result<Response, String> {
    let settings = keys
        .settings
        .oidc_enabled()
        .ok_or("Single sign-on isn't turned on.")?;
    if login.expires <= now() {
        return Err("This sign-in has expired. Try again.".to_string());
    }
    if !params
        .state
        .as_deref()
        .is_some_and(|sent| same(sent, &login.state))
    {
        return Err("This sign-in doesn't match the one that was started.".to_string());
    }
    if let Some(error) = params.error {
        let detail = params.error_description.unwrap_or_default();
        return Err(format!("The provider said: {error} {detail}")
            .trim()
            .to_string());
    }
    let code = params.code.ok_or("The provider sent no code.")?;

    let http = state.auth.oidc_http();
    let provider = oidc::discover(http, &settings.issuer).await?;
    let client_secret = keys
        .oidc_secret
        .as_deref()
        .ok_or("No client secret is set.")?;
    let id_token = oidc::exchange(
        http,
        &provider,
        &oidc::Exchange {
            client_id: &settings.client_id,
            client_secret,
            code: &code,
            redirect_uri: &login.redirect_uri,
            verifier: &login.verifier,
        },
    )
    .await?;
    let claims = oidc::verify(
        http,
        &provider,
        &settings.client_id,
        &id_token,
        &login.nonce,
    )
    .await?;

    let groups = claims.groups(&settings.groups_claim)?;
    let user = claims.display();
    let Some(role) = settings.roles.role_for(&groups) else {
        return Err(if groups.is_empty() {
            format!(
                "{user} has no groups in the {} claim, so has no role here.",
                settings.groups_claim
            )
        } else {
            format!(
                "None of {user}'s groups has a role here. The provider sent: {}.",
                groups.join(", ")
            )
        });
    };
    let auth_at = if login.step_up {
        check_step_up(claims.auth_time())?
    } else {
        claims.auth_time().unwrap_or(0)
    };

    let (sealed, _) = state.auth.issue(keys, &user, "oidc", role, auth_at)?;
    info!("Console: {user} signed in through OIDC as {role:?}");
    let secure = secure(headers);
    let session = cookie(SESSION_COOKIE, &sealed, SESSION_SECS, "Strict", secure)
        .map_err(|_| "can't build the session cookie".to_string())?;
    let spent = cookie(OIDC_COOKIE, "", 0, "Lax", secure)
        .map_err(|_| "can't build the login cookie".to_string())?;
    let target = if login.step_up {
        let glue = if login.return_to.contains('?') {
            '&'
        } else {
            '?'
        };
        format!("{}{glue}reauthed=1", login.return_to)
    } else {
        login.return_to.clone()
    };
    let mut response = Redirect::to(&target).into_response();
    response.headers_mut().append(header::SET_COOKIE, session);
    response.headers_mut().append(header::SET_COOKIE, spent);
    Ok(response)
}

pub async fn oidc_callback(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<CallbackParams>,
) -> Response {
    let keys = state.auth.keys();
    let login = cookie_value(&headers, OIDC_COOKIE)
        .and_then(|sealed| open::<OidcLogin>(&keys.session_key, sealed));
    let Some(login) = login else {
        return login_failed("This sign-in has expired. Try again.");
    };
    match finish_oidc(&state, &keys, &headers, &login, params).await {
        Ok(response) => response,
        Err(message) if login.step_up => step_up_failed(&login.return_to, &message),
        Err(message) => login_failed(&message),
    }
}

#[cfg(test)]
impl Auth {
    /// The same session, as if its sign-in were `ago` seconds older.
    pub fn aged(&self, cookie: &str, ago: u64) -> String {
        let keys = self.keys();
        let sealed = cookie.split_once('=').map_or(cookie, |(_, value)| value);
        let mut session: Session = open(&keys.session_key, sealed).expect("a session");
        session.auth_at -= ago;
        format!(
            "{SESSION_COOKIE}={}",
            seal(&keys.session_key, &session).unwrap()
        )
    }

    pub fn session_for(&self, user: &str, role: Role) -> String {
        let keys = self.keys();
        let (sealed, _) = self.issue(&keys, user, "oidc", role, now()).unwrap();
        format!("{SESSION_COOKIE}={sealed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_password_is_a_hash() {
        let stored = hash_password("correct horse battery").unwrap();
        assert!(stored.starts_with("$argon2id$"), "{stored}");
        assert!(!stored.contains("correct"));
        assert!(password_matches("correct horse battery", &stored));
        assert!(!password_matches("wrong horse battery", &stored));
        assert!(!password_matches("anything", "not a hash"));
    }

    #[test]
    fn a_sealed_value_can_only_be_opened_unchanged_with_its_key() {
        let sealed = seal(b"key", &json!({ "user": "a" })).unwrap();
        assert_eq!(open::<Value>(b"key", &sealed), Some(json!({ "user": "a" })));
        assert_eq!(open::<Value>(b"other", &sealed), None);

        let (payload, signature) = sealed.split_once('.').unwrap();
        let forged = URL_SAFE_NO_PAD.encode(br#"{"user":"b"}"#);
        assert_eq!(
            open::<Value>(b"key", &format!("{forged}.{signature}")),
            None
        );
        assert_eq!(open::<Value>(b"key", payload), None);
    }

    #[test]
    fn cookies_are_found_among_others() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; mmx_session=abc; b=2"),
        );
        assert_eq!(cookie_value(&headers, SESSION_COOKIE), Some("abc"));
        assert_eq!(cookie_value(&headers, "missing"), None);
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, HeaderValue::from_static(value));
        }
        headers
    }

    #[test]
    fn origins_must_match_the_host_the_client_used() {
        let behind = |origin: &'static str| {
            headers(&[
                ("host", "mmx-console:8093"),
                ("x-forwarded-host", "mmx.ops.internal"),
                ("origin", origin),
            ])
        };
        assert!(check_origin(&behind("http://mmx-console:8093")).is_ok());
        assert!(check_origin(&behind("https://mmx.ops.internal")).is_ok());
        assert!(check_origin(&behind("https://evil.example")).is_err());
        assert!(check_origin(&HeaderMap::new()).is_ok());
    }

    #[test]
    fn the_redirect_uri_comes_from_the_request() {
        assert_eq!(
            redirect_uri(&headers(&[("host", "127.0.0.1:8093")])).unwrap(),
            "http://127.0.0.1:8093/admin/v1/oidc/callback"
        );
        assert_eq!(
            redirect_uri(&headers(&[
                ("host", "mmx-console:8093"),
                ("x-forwarded-host", "mmx.ops.internal, edge.internal"),
                ("x-forwarded-proto", "https, http"),
            ]))
            .unwrap(),
            "https://mmx.ops.internal/admin/v1/oidc/callback"
        );
    }

    #[test]
    fn a_junk_forwarded_host_falls_back_to_the_real_one() {
        let sent = headers(&[
            ("host", "mmx-console:8093"),
            ("x-forwarded-host", "evil.example/path?x=1"),
        ]);
        assert_eq!(request_origin(&sent).unwrap(), "http://mmx-console:8093");
    }

    #[test]
    fn cookies_are_secure_only_behind_tls() {
        assert!(secure(&headers(&[("x-forwarded-proto", "https")])));
        assert!(!secure(&headers(&[("x-forwarded-proto", "http")])));
        assert!(!secure(&HeaderMap::new()));
    }

    #[test]
    fn the_highest_listed_role_wins_and_nothing_else_gets_in() {
        let roles = RoleGroups {
            admin: vec!["ops-admin".to_string()],
            operator: vec!["ops".to_string()],
            viewer: vec!["support".to_string()],
        };
        let of = |names: &[&str]| {
            let names: Vec<String> = names.iter().map(ToString::to_string).collect();
            roles.role_for(&names)
        };
        assert_eq!(of(&["support", "ops-admin"]), Some(Role::Admin));
        assert_eq!(of(&["ops"]), Some(Role::Operator));
        assert_eq!(of(&["support"]), Some(Role::Viewer));
        assert_eq!(of(&["everyone"]), None);
        assert_eq!(of(&[]), None);
        assert_eq!(of(&["OPS"]), None, "names match exactly");
    }

    #[test]
    fn each_route_needs_the_role_the_table_says() {
        let role = required_role;
        assert_eq!(role(&Method::GET, "/admin/v1/markets"), Role::Viewer);
        assert_eq!(role(&Method::GET, "/events"), Role::Viewer);
        assert_eq!(role(&Method::GET, "/settings"), Role::Admin);
        assert_eq!(role(&Method::POST, "/markets/IRON/halt"), Role::Operator);
        assert_eq!(role(&Method::DELETE, "/markets/IRON"), Role::Operator);
        assert_eq!(role(&Method::PUT, "/config/markets"), Role::Operator);
        assert_eq!(role(&Method::POST, "/webhooks/secret"), Role::Admin);
        assert_eq!(role(&Method::PUT, "/webhooks/config"), Role::Admin);
        assert_eq!(role(&Method::PUT, "/settings/oidc"), Role::Admin);
        assert_eq!(role(&Method::POST, "/something/new"), Role::Admin);
    }

    #[test]
    fn step_up_covers_the_destructive_routes_and_no_others() {
        let up = needs_step_up;
        assert!(up(&Method::DELETE, "/admin/v1/markets/IRON"));
        assert!(up(&Method::POST, "/markets/IRON/cancel-all"));
        assert!(up(&Method::POST, "/webhooks/secret"));
        assert!(up(&Method::PUT, "/settings"));
        assert!(up(&Method::PUT, "/settings/oidc"));
        assert!(up(&Method::POST, "/sessions/revoke"));
        assert!(!up(&Method::POST, "/markets/IRON/halt"));
        assert!(!up(&Method::PUT, "/markets/IRON"));
        assert!(!up(&Method::GET, "/settings"));
    }

    #[test]
    fn a_confirmation_needs_a_recent_sign_in_from_the_provider() {
        assert!(check_step_up(None).is_err());
        assert!(check_step_up(Some(now() - 3600)).is_err());
        assert!(check_step_up(Some(now())).is_ok());
    }

    #[test]
    fn landing_pages_stay_on_this_server() {
        assert_eq!(local_path(Some("/markets/IRON")), "/markets/IRON");
        assert_eq!(local_path(Some("//evil.example")), "/");
        assert_eq!(local_path(Some("/\\evil.example")), "/");
        assert_eq!(local_path(Some("https://evil.example")), "/");
        assert_eq!(local_path(None), "/");
    }

    #[test]
    fn a_provider_from_the_environment_must_be_complete() {
        let read = |pairs: &'static [(&str, &str)]| {
            Managed::read(|name| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            })
        };
        assert_eq!(read(&[]).unwrap(), Managed::default());
        assert_eq!(
            read(&[("MMX_PASSWORD_LOGIN", "false")])
                .unwrap()
                .password_login,
            Some(false)
        );
        assert!(read(&[("MMX_PASSWORD_LOGIN", "maybe")]).is_err());
        assert!(read(&[("MMX_OIDC_ISSUER", "https://id.example")]).is_err());
        assert!(
            read(&[
                ("MMX_OIDC_ISSUER", "https://id.example"),
                ("MMX_OIDC_CLIENT_ID", "mmx"),
                ("MMX_OIDC_CLIENT_SECRET", "s"),
            ])
            .unwrap_err()
            .contains("ADMIN_GROUPS")
        );
        let full = read(&[
            ("MMX_OIDC_ISSUER", "https://id.example/"),
            ("MMX_OIDC_CLIENT_ID", "mmx"),
            ("MMX_OIDC_CLIENT_SECRET", "s"),
            ("MMX_OIDC_ADMIN_GROUPS", "a, b"),
            ("MMX_OIDC_GROUPS_CLAIM", "roles"),
        ])
        .unwrap();
        let oidc = full.oidc.unwrap();
        assert_eq!(oidc.issuer, "https://id.example");
        assert_eq!(oidc.groups_claim, "roles");
        assert_eq!(oidc.roles.admin, ["a", "b"]);
    }

    #[test]
    fn an_older_settings_file_still_loads() {
        let text = "public_url: https://old.example\npassword_login: true\noidc:\n  enabled: true\n  issuer: https://id.example\n  client_id: mmx\n  allowed: ['a@b.example']\n";
        let settings: Settings = serde_yaml::from_str(text).unwrap();
        let oidc = settings.oidc.unwrap();
        assert_eq!(oidc.groups_claim, "groups");
        assert_eq!(
            oidc.roles,
            RoleGroups::default(),
            "the old list grants nothing"
        );
    }

    #[test]
    fn hints_show_only_the_end() {
        assert_eq!(hint("abcdefgh"), "...efgh");
        assert_eq!(hint("ab"), "...ab");
    }

    #[test]
    fn short_passwords_are_refused() {
        assert!(check_new_password("short").is_err());
        assert!(check_new_password("twelve chars").is_ok());
    }
}

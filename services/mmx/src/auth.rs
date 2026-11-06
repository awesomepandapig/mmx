//! Who may use `/admin/v1`. The CLI sends `MMX_ADMIN_TOKEN` as a bearer, and the console holds a
//! signed session cookie from a password or an OIDC provider.
//!
//! There's no default password. The first start reads `MMX_ADMIN_PASSWORD` or generates one and logs
//! it once, and only a hash is stored. That password only unlocks first-run setup, where the admin
//! chooses a real one.
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
pub const CSRF_HEADER: &str = "x-csrf-token";

const SESSION_SECS: u64 = 12 * 60 * 60;
const OIDC_LOGIN_SECS: u64 = 10 * 60;
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

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct OidcSettings {
    pub enabled: bool,
    pub issuer: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Emails, `@domains` or subjects. Nobody gets in if this is empty.
    #[serde(default)]
    pub allowed: Vec<String>,
}

fn yes() -> bool {
    true
}

/// `console.yaml` in the store. None of it is secret.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    #[serde(default = "yes")]
    pub password_login: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc: Option<OidcSettings>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            public_url: None,
            password_login: true,
            oidc: None,
        }
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
    oidc_secret: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub user: String,
    pub method: String,
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

pub struct Auth {
    store: Arc<ConfigStore>,
    /// Hashed, so that comparing doesn't reveal the token's length.
    token: Option<[u8; 32]>,
    insecure_local: bool,
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

/// Secure cookies need TLS, which the operator terminates in front of this server, so the public URL
/// says whether it's there. Without one, a proxy's header can still turn it on.
fn secure(keys: &Keys, headers: &HeaderMap) -> bool {
    let public = keys
        .settings
        .public_url
        .as_deref()
        .is_some_and(|url| url.starts_with("https://"));
    let forwarded = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|proto| proto.eq_ignore_ascii_case("https"));
    public || forwarded
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
/// login, which has no session to hold one yet.
fn check_origin(keys: &Keys, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let origin = origin.to_str().unwrap_or_default();
    let host = origin.split_once("://").map_or(origin, |(_, host)| host);
    let host_matches = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|expected| expected.eq_ignore_ascii_case(host));
    let public_matches = keys
        .settings
        .public_url
        .as_deref()
        .is_some_and(|url| url.trim_end_matches('/').eq_ignore_ascii_case(origin));
    if host_matches || public_matches {
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
    ) -> Result<Arc<Self>, String> {
        let auth = Arc::new(Self {
            store,
            token: token.map(digest),
            insecure_local,
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
        match self.store.put_secrets(&doc).await {
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
        let keys = Keys {
            session_key: field(SESSION_KEY_KEY)
                .map(String::into_bytes)
                .ok_or("the Secret has no session key")?,
            generation: field(GENERATION_KEY)
                .and_then(|text| text.parse().ok())
                .unwrap_or(0),
            password_hash: field(PASSWORD_HASH_KEY),
            must_set: field(MUST_SET_KEY).is_some_and(|text| text == "true"),
            settings: Settings::read(&config.data)?,
            oidc_secret: field(OIDC_SECRET_KEY),
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

    fn issue(&self, keys: &Keys, user: &str, method: &str) -> Result<(String, Session), String> {
        let session = Session {
            user: user.to_string(),
            method: method.to_string(),
            expires: now() + SESSION_SECS,
            generation: keys.generation,
            csrf: random_text(24)?,
        };
        Ok((seal(&keys.session_key, &session)?, session))
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
                check_origin(&keys, headers)?;
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

/// Everything under `/admin/v1`, apart from signing in.
pub async fn require_admin(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    match state
        .auth
        .identify(request.method(), request.headers(), false)
    {
        Ok(caller) => {
            request.extensions_mut().insert(caller);
            next.run(request).await
        }
        Err(err) => refuse(err),
    }
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
        "must_set_password": session.is_some_and(|session| session.method == "password") && keys.must_set,
        "password_login": keys.settings.password_login,
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
        secure(keys, headers),
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
    check_origin(&keys, &headers)?;
    if !keys.settings.password_login {
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
        .issue(&keys, "admin", "password")
        .map_err(internal)?;
    with_session(&state, &keys, &headers, &sealed, &session)
}

pub async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let keys = state.auth.keys();
    let value = cookie(SESSION_COOKIE, "", 0, "Strict", secure(&keys, &headers))?;
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
        .issue(&keys, &session.user, &session.method)
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
                .issue(&keys, &session.user, &session.method)
                .map_err(internal)?;
            with_session(&state, &keys, &headers, &sealed, &session)
        }
        _ => Ok(Json(json!({ "revoked": true })).into_response()),
    }
}

/// What the browser carries while it's away at the provider.
#[derive(Serialize, Deserialize, Debug)]
struct OidcLogin {
    state: String,
    nonce: String,
    verifier: String,
    expires: u64,
}

fn login_failed(message: &str) -> Response {
    warn!("Console: OIDC login failed: {message}");
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("error", message)
        .finish();
    Redirect::to(&format!("/login?{query}")).into_response()
}

fn oidc_redirect_uri(settings: &Settings) -> Option<String> {
    let public = settings.public_url.as_deref()?;
    Some(format!(
        "{}/admin/v1/oidc/callback",
        public.trim_end_matches('/')
    ))
}

pub fn redirect_uri(settings: &Settings) -> Option<String> {
    oidc_redirect_uri(settings)
}

async fn start_oidc(
    state: &AppState,
    keys: &Keys,
    headers: &HeaderMap,
) -> Result<Response, String> {
    let settings = keys
        .settings
        .oidc_enabled()
        .ok_or("Single sign-on isn't turned on.")?;
    let redirect = oidc_redirect_uri(&keys.settings).ok_or("The public URL isn't set.")?;
    let provider = oidc::discover(state.auth.oidc_http(), &settings.issuer).await?;

    let login = OidcLogin {
        state: random_text(24)?,
        nonce: random_text(24)?,
        verifier: random_text(48)?,
        expires: now() + OIDC_LOGIN_SECS,
    };
    let address = oidc::authorize_url(
        &provider,
        &oidc::Request {
            client_id: &settings.client_id,
            redirect_uri: &redirect,
            state: &login.state,
            nonce: &login.nonce,
            verifier: &login.verifier,
        },
    )?;
    let sealed = seal(&keys.session_key, &login)?;
    // Lax, because the provider's redirect back is a navigation from another site.
    let value = cookie(
        OIDC_COOKIE,
        &sealed,
        OIDC_LOGIN_SECS,
        "Lax",
        secure(keys, headers),
    )
    .map_err(|_| "can't build the login cookie".to_string())?;
    let mut response = Redirect::to(&address).into_response();
    response.headers_mut().insert(header::SET_COOKIE, value);
    Ok(response)
}

pub async fn oidc_start(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let keys = state.auth.keys();
    match start_oidc(&state, &keys, &headers).await {
        Ok(response) => response,
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

async fn finish_oidc(
    state: &AppState,
    keys: &Keys,
    headers: &HeaderMap,
    params: CallbackParams,
) -> Result<Response, String> {
    let settings = keys
        .settings
        .oidc_enabled()
        .ok_or("Single sign-on isn't turned on.")?;
    let sealed =
        cookie_value(headers, OIDC_COOKIE).ok_or("This sign-in has expired. Try again.")?;
    let login: OidcLogin =
        open(&keys.session_key, sealed).ok_or("This sign-in has expired. Try again.")?;
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
    let redirect = oidc_redirect_uri(&keys.settings).ok_or("The public URL isn't set.")?;
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
            redirect_uri: &redirect,
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
    if !claims.allowed(&settings.allowed) {
        return Err(format!(
            "{} isn't allowed to use this console.",
            claims.display()
        ));
    }

    let (sealed, _) = state.auth.issue(keys, &claims.display(), "oidc")?;
    info!("Console: {} signed in through OIDC", claims.display());
    let secure = secure(keys, headers);
    let session = cookie(SESSION_COOKIE, &sealed, SESSION_SECS, "Strict", secure)
        .map_err(|_| "can't build the session cookie".to_string())?;
    let spent = cookie(OIDC_COOKIE, "", 0, "Lax", secure)
        .map_err(|_| "can't build the login cookie".to_string())?;
    let mut response = Redirect::to("/").into_response();
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
    match finish_oidc(&state, &keys, &headers, params).await {
        Ok(response) => response,
        Err(message) => login_failed(&message),
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

    #[test]
    fn origins_must_match_the_host_or_public_url() {
        let keys = Keys {
            settings: Settings {
                public_url: Some("https://console.example/".to_string()),
                ..Settings::default()
            },
            ..Keys::default()
        };
        let headers = |origin: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, HeaderValue::from_static("mmx:8093"));
            headers.insert(header::ORIGIN, HeaderValue::from_static(origin));
            headers
        };
        assert!(check_origin(&keys, &headers("http://mmx:8093")).is_ok());
        assert!(check_origin(&keys, &headers("https://console.example")).is_ok());
        assert!(check_origin(&keys, &headers("https://evil.example")).is_err());
        assert!(check_origin(&keys, &HeaderMap::new()).is_ok());
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

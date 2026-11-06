//! What the console's settings and webhook pages read and write. Only settings that take effect are
//! here. Secrets are written but never read back, apart from a webhook secret at the moment it's
//! generated, which is the only time anyone can see it.

use crate::auth::{self, Caller, OidcSettings, hint, random_hex, update_settings};
use crate::oidc;
use crate::server::{ApiError, AppState, error, invalid, store_failed};
use crate::store::{StoreError, WEBHOOK_SECRET_KEY, WEBHOOK_URL_KEY};

use std::sync::Arc;

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::info;
use url::Url;

fn oidc_view(state: &AppState) -> Value {
    let keys = state.auth.keys();
    let settings = &keys.settings;
    let oidc = settings.oidc.clone().unwrap_or_default();
    json!({
        "enabled": oidc.enabled,
        "issuer": oidc.issuer,
        "client_id": oidc.client_id,
        "display_name": oidc.display_name,
        "allowed": oidc.allowed,
        "client_secret_hint": state.auth.oidc_secret_hint(),
        "redirect_uri": auth::redirect_uri(settings),
    })
}

pub async fn get_settings(State(state): State<Arc<AppState>>) -> Json<Value> {
    let keys = state.auth.keys();
    Json(json!({
        "public_url": keys.settings.public_url,
        "password_login": keys.settings.password_login,
        "oidc": oidc_view(&state),
        "store": state.store.info(),
        "admin_token_set": state.auth.has_token(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsBody {
    public_url: Option<String>,
    password_login: Option<bool>,
}

fn check_public_url(text: &str) -> Result<String, ApiError> {
    let url = Url::parse(text).map_err(|err| invalid(format!("That isn't a URL: {err}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err(invalid(
            "The public URL must start with http:// or https://.",
        ));
    }
    if url.path() != "/" || url.query().is_some() {
        return Err(invalid(
            "The public URL is only the scheme and host, because the console runs at the root.",
        ));
    }
    Ok(text.trim_end_matches('/').to_string())
}

/// Turning passwords off can lock everyone out, so it needs proof that sign-on works: the person
/// asking must have signed in with it just now. The token is the way back in either way.
fn check_password_login(state: &AppState, caller: &Caller, on: bool) -> Result<(), ApiError> {
    if on {
        return Ok(());
    }
    let keys = state.auth.keys();
    if keys.settings.oidc_enabled().is_none() {
        return Err(invalid(
            "Turn on single sign-on before turning off password login.",
        ));
    }
    match caller {
        Caller::Session(session) if session.method != "oidc" => Err(invalid(
            "Sign in with single sign-on first, to prove it works, then turn off password login.",
        )),
        _ => Ok(()),
    }
}

pub async fn put_settings(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    body: Result<Json<SettingsBody>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(body) = body.map_err(|rejection| invalid(rejection.body_text()))?;
    let public_url = match body.public_url.as_deref() {
        Some("") | None => None,
        Some(text) => Some(check_public_url(text)?),
    };
    let clear_url = body.public_url.as_deref() == Some("");
    if let Some(on) = body.password_login {
        check_password_login(&state, &caller, on)?;
    }

    let _edit = state.edits.lock().await;
    update_settings(&state.store, |settings| {
        if let Some(url) = &public_url {
            settings.public_url = Some(url.clone());
        }
        if clear_url {
            settings.public_url = None;
        }
        if let Some(on) = body.password_login {
            settings.password_login = on;
        }
        Ok(())
    })
    .await
    .map_err(store_failed)?;
    state
        .auth
        .refresh()
        .await
        .map_err(|message| store_failed(StoreError::Failed(message)))?;
    Ok(get_settings(State(state.clone())).await)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcBody {
    enabled: bool,
    issuer: String,
    client_id: String,
    /// Left out, the stored one stays.
    client_secret: Option<String>,
    display_name: Option<String>,
    #[serde(default)]
    allowed: Vec<String>,
}

pub async fn put_oidc(
    State(state): State<Arc<AppState>>,
    body: Result<Json<OidcBody>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(body) = body.map_err(|rejection| invalid(rejection.body_text()))?;
    let keys = state.auth.keys();
    let client_secret = body.client_secret.filter(|secret| !secret.is_empty());
    let allowed: Vec<String> = body
        .allowed
        .iter()
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect();

    if !body.issuer.is_empty() {
        oidc::check_url("The issuer", body.issuer.trim()).map_err(invalid)?;
    }
    if body.enabled {
        if body.issuer.trim().is_empty() || body.client_id.trim().is_empty() {
            return Err(invalid("Single sign-on needs an issuer and a client ID."));
        }
        if client_secret.is_none() && state.auth.oidc_secret_hint().is_none() {
            return Err(invalid("Single sign-on needs a client secret."));
        }
        if allowed.is_empty() {
            return Err(invalid(
                "List who may sign in. Otherwise any account at the provider could become an admin.",
            ));
        }
        if keys.settings.public_url.is_none() {
            return Err(invalid(
                "Set the public URL first, because the provider needs a redirect URI.",
            ));
        }
    } else if !keys.settings.password_login {
        return Err(invalid(
            "Turn on password login first, or nobody could sign in.",
        ));
    }

    let oidc = OidcSettings {
        enabled: body.enabled,
        issuer: body.issuer.trim().trim_end_matches('/').to_string(),
        client_id: body.client_id.trim().to_string(),
        display_name: body.display_name.filter(|name| !name.trim().is_empty()),
        allowed,
    };
    let _edit = state.edits.lock().await;
    if let Some(secret) = &client_secret {
        state.auth.set_oidc_secret(secret).await?;
    }
    update_settings(&state.store, |settings| {
        settings.oidc = Some(oidc.clone());
        Ok(())
    })
    .await
    .map_err(store_failed)?;
    state
        .auth
        .refresh()
        .await
        .map_err(|message| store_failed(StoreError::Failed(message)))?;
    Ok(Json(oidc_view(&state)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcTestBody {
    issuer: String,
}

/// Fetches the provider's discovery document, which proves the address is right and what's
/// there is an OIDC provider. It can't prove the client ID or secret. Only a real sign-in does that.
pub async fn test_oidc(
    State(state): State<Arc<AppState>>,
    body: Result<Json<OidcTestBody>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(body) = body.map_err(|rejection| invalid(rejection.body_text()))?;
    match oidc::discover(state.auth.oidc_http(), body.issuer.trim()).await {
        Ok(provider) => Ok(Json(json!({
            "ok": true,
            "issuer": provider.issuer,
            "authorization_endpoint": provider.authorization_endpoint,
            "token_endpoint": provider.token_endpoint,
        }))),
        Err(message) => Ok(Json(json!({ "ok": false, "error": message }))),
    }
}

/// Matches `config_id` in the outbox's `/position`, so the two can be compared without either
/// showing the secret.
pub fn webhook_fingerprint(url: &str, secret: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(url.as_bytes());
    hash.update([0]);
    hash.update(secret.as_bytes());
    hash.finalize()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What a saved webhook has reached. The outbox reads its settings once, at start.
fn webhook_state(
    url: Option<&str>,
    secret: Option<&str>,
    running: Option<&crate::upstream::Position>,
) -> &'static str {
    let (Some(url), Some(secret)) = (url, secret) else {
        return match running {
            // It works, with settings from its own environment that this server didn't write.
            Some(position)
                if position.configured != Some(false) && position.webhook_url.is_some() =>
            {
                "from_environment"
            }
            _ => "incomplete",
        };
    };
    // One that started with no webhook certainly isn't using this one.
    if running.is_some_and(|position| position.configured == Some(false)) {
        return "restart_required";
    }
    match running.and_then(|position| position.config_id.as_deref()) {
        Some(id) if id == webhook_fingerprint(url, secret) => "applied",
        Some(_) => "restart_required",
        // An outbox that can't be reached, or is too old to say, proves nothing either way.
        None => "unknown",
    }
}

pub async fn webhook_config(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let config = state.store.config().await.map_err(store_failed)?;
    let secrets = state.store.secrets().await.map_err(store_failed)?;
    let url = config.data.get(WEBHOOK_URL_KEY).map(String::as_str);
    let secret = secrets.data.get(WEBHOOK_SECRET_KEY).map(String::as_str);
    let running = state.up.position().await;
    let running_error = running.as_ref().err().cloned();
    let running = running.ok();
    Ok(Json(json!({
        "url": url,
        "secret_hint": secret.map(hint),
        "state": webhook_state(url, secret, running.as_ref()),
        "running_url": running.as_ref().and_then(|position| position.webhook_url.clone()),
        "running_error": running_error,
        "restart": state.outbox_restart,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookBody {
    url: String,
}

fn check_webhook_url(text: &str) -> Result<(), ApiError> {
    let url = Url::parse(text).map_err(|err| invalid(format!("That isn't a URL: {err}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err(invalid(
            "The webhook URL must start with http:// or https://.",
        ));
    }
    Ok(())
}

pub async fn put_webhook_config(
    State(state): State<Arc<AppState>>,
    body: Result<Json<WebhookBody>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(body) = body.map_err(|rejection| invalid(rejection.body_text()))?;
    let url = body.url.trim().to_string();
    check_webhook_url(&url)?;
    let _edit = state.edits.lock().await;
    state
        .store
        .update_config(|data| {
            data.insert(WEBHOOK_URL_KEY.to_string(), url.clone());
            Ok(())
        })
        .await
        .map_err(store_failed)?;
    info!("Webhooks: URL set to {url}");
    webhook_config(State(state.clone())).await
}

/// Generates a signing secret. This answer is the only place it ever appears.
pub async fn rotate_webhook_secret(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, ApiError> {
    let secret = random_hex()
        .map_err(|message| error(StatusCode::INTERNAL_SERVER_ERROR, "internal", message))?;
    let _edit = state.edits.lock().await;
    state
        .store
        .update_secrets(|data| {
            data.insert(WEBHOOK_SECRET_KEY.to_string(), secret.clone());
            Ok(())
        })
        .await
        .map_err(store_failed)?;
    info!("Webhooks: a new signing secret was generated");
    let Json(mut view) = webhook_config(State(state.clone())).await?;
    view["secret"] = json!(secret);
    Ok(Json(view))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::Position;

    fn position(id: Option<&str>, url: Option<&str>, configured: Option<bool>) -> Position {
        Position {
            durable_position: 0,
            durable_exec_id: 0,
            delivered_position: 0,
            verified: true,
            halted: None,
            consecutive_failures: 0,
            webhook_url: url.map(str::to_string),
            config_id: id.map(str::to_string),
            configured,
        }
    }

    #[test]
    fn webhook_state_only_claims_what_the_outbox_proves() {
        let (url, secret) = (Some("http://game"), Some("secret"));
        let id = webhook_fingerprint("http://game", "secret");
        let matching = position(Some(&id), url, Some(true));
        assert_eq!(webhook_state(url, secret, Some(&matching)), "applied");

        let stale = position(Some("different"), url, Some(true));
        assert_eq!(webhook_state(url, secret, Some(&stale)), "restart_required");

        let unconfigured = position(None, None, Some(false));
        assert_eq!(
            webhook_state(url, secret, Some(&unconfigured)),
            "restart_required"
        );

        let old_build = position(None, None, None);
        assert_eq!(webhook_state(url, secret, Some(&old_build)), "unknown");
        assert_eq!(webhook_state(url, secret, None), "unknown");

        assert_eq!(webhook_state(url, None, None), "incomplete");
        let own = position(Some("x"), url, Some(true));
        assert_eq!(webhook_state(None, None, Some(&own)), "from_environment");
    }

    // From Python's hashlib, and the same value is checked in `services/outbox`:
    // hashlib.sha256(b"https://game.example/fills\0secret").hexdigest()[:16]
    #[test]
    fn the_fingerprint_matches_the_outbox() {
        assert_eq!(
            webhook_fingerprint("https://game.example/fills", "secret"),
            "7beb90646192c9b7"
        );
    }

    #[test]
    fn the_fingerprint_changes_with_either_value() {
        let base = webhook_fingerprint("a", "b");
        assert_ne!(base, webhook_fingerprint("a", "c"));
        assert_ne!(base, webhook_fingerprint("c", "b"));
        // The separator stops the two values running together.
        assert_ne!(
            webhook_fingerprint("ab", "c"),
            webhook_fingerprint("a", "bc")
        );
    }

    #[test]
    fn a_public_url_is_only_scheme_and_host() {
        assert_eq!(
            check_public_url("https://console.example/").unwrap(),
            "https://console.example"
        );
        assert!(check_public_url("http://localhost:8093").is_ok());
        assert!(check_public_url("https://console.example/mmx").is_err());
        assert!(check_public_url("ftp://console.example").is_err());
    }
}

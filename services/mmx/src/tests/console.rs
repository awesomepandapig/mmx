//! The console's server side, driven over HTTP: sign-in, the markets editor, a fake Kubernetes API
//! and a fake OIDC issuer. The fakes follow the documented behaviour of the real ones. Nothing here
//! has talked to a real cluster or provider.

use super::*;

use crate::auth::{self, CSRF_HEADER, SESSION_COOKIE};
use crate::oidc;
use crate::settings::webhook_fingerprint;
use crate::store::{Doc, KubeStore, MARKETS_KEY, StoreError};

use axum::Form;
use axum::http::HeaderMap;
use axum::routing::post;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub const FIRST_PASSWORD: &str = "first-run-password";
const REAL_PASSWORD: &str = "a much better password";

/// A browser, as far as this server can tell: it keeps a cookie and a CSRF token.
struct Browser {
    url: String,
    http: reqwest::Client,
    cookie: String,
    csrf: String,
}

fn session_cookie(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with(SESSION_COOKIE))
        .map(str::to_string)
}

fn cookie_pair(header: &str) -> String {
    header.split(';').next().unwrap().to_string()
}

impl World {
    fn browser(&self) -> Browser {
        Browser {
            url: self.url.clone(),
            http: self.http.clone(),
            cookie: String::new(),
            csrf: String::new(),
        }
    }

    async fn signed_in(&self, password: &str) -> Browser {
        let mut browser = self.browser();
        let (code, body) = browser.login(password).await;
        assert_eq!(code, 200, "{body}");
        browser
    }

    /// Past first-run setup, with a real password.
    async fn admin(&self) -> Browser {
        let mut browser = self.signed_in(FIRST_PASSWORD).await;
        let body = json!({ "new": REAL_PASSWORD });
        let (code, reply) = browser.post("/password", Some(body)).await;
        assert_eq!(code, 200, "{reply}");
        browser
    }
}

impl Browser {
    async fn take(&mut self, response: reqwest::Response) -> (u16, Value) {
        if let Some(header) = session_cookie(&response) {
            self.cookie = cookie_pair(&header);
        }
        let code = response.status().as_u16();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if let Some(token) = body.get("csrf_token").and_then(Value::as_str) {
            self.csrf = token.to_string();
        }
        (code, body)
    }

    async fn login(&mut self, password: &str) -> (u16, Value) {
        let response = self
            .http
            .post(format!("{}/admin/v1/login", self.url))
            .json(&json!({ "password": password }))
            .send()
            .await
            .unwrap();
        self.take(response).await
    }

    async fn get(&mut self, path: &str) -> (u16, Value) {
        let response = self
            .http
            .get(format!("{}/admin/v1{path}", self.url))
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap();
        self.take(response).await
    }

    async fn send(&mut self, method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut request = self
            .http
            .request(method, format!("{}/admin/v1{path}", self.url))
            .header("cookie", &self.cookie)
            .header(CSRF_HEADER, &self.csrf);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        self.take(response).await
    }

    async fn post(&mut self, path: &str, body: Option<Value>) -> (u16, Value) {
        self.send(Method::POST, path, body).await
    }

    async fn put(&mut self, path: &str, body: Value) -> (u16, Value) {
        self.send(Method::PUT, path, Some(body)).await
    }
}

async fn plain_world() -> World {
    world(&[FakeGateway::new(&temp_markets(FILE))]).await
}

#[tokio::test]
async fn the_first_password_is_stored_only_as_a_hash() {
    let world = plain_world().await;
    let text = fs::read_to_string(world.dir.join("secrets.json")).unwrap();
    assert!(text.contains("$argon2id$"), "{text}");
    assert!(!text.contains(FIRST_PASSWORD), "{text}");
}

#[cfg(unix)]
#[tokio::test]
async fn local_secrets_are_private_to_their_owner() {
    use std::os::unix::fs::PermissionsExt;
    let world = plain_world().await;
    let mode = fs::metadata(world.dir.join("secrets.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test]
async fn a_restart_keeps_the_password_it_has() {
    let world = plain_world().await;
    let before = fs::read_to_string(world.dir.join("secrets.json")).unwrap();
    Auth::start(
        world.state.store.clone(),
        Some(TOKEN),
        false,
        Some("other".to_string()),
    )
    .await
    .unwrap();
    let after = fs::read_to_string(world.dir.join("secrets.json")).unwrap();
    assert_eq!(before, after);
}

#[tokio::test]
async fn with_no_password_set_one_is_generated() {
    let path = temp_markets(FILE);
    let dir = path.with_file_name("state");
    let store = Arc::new(ConfigStore::Local(LocalStore {
        markets_file: path,
        dir: dir.clone(),
    }));
    Auth::start(store, Some(TOKEN), false, None).await.unwrap();
    let text = fs::read_to_string(dir.join("secrets.json")).unwrap();
    assert!(text.contains("$argon2id$"), "{text}");
}

#[tokio::test]
async fn a_session_cookie_is_http_only_and_same_site() {
    let world = plain_world().await;
    let response = world
        .http
        .post(format!("{}/admin/v1/login", world.url))
        .json(&json!({ "password": FIRST_PASSWORD }))
        .send()
        .await
        .unwrap();
    let cookie = session_cookie(&response).unwrap();
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    assert!(
        !cookie.contains("Secure"),
        "plain HTTP can't use it: {cookie}"
    );
}

#[tokio::test]
async fn a_public_https_url_makes_cookies_secure() {
    let world = plain_world().await;
    let mut admin = world.admin().await;
    let (code, body) = admin
        .put(
            "/settings",
            json!({ "public_url": "https://console.example" }),
        )
        .await;
    assert_eq!(code, 200, "{body}");
    let response = world
        .http
        .post(format!("{}/admin/v1/login", world.url))
        .json(&json!({ "password": REAL_PASSWORD }))
        .send()
        .await
        .unwrap();
    assert!(session_cookie(&response).unwrap().contains("Secure"));
}

#[tokio::test]
async fn the_wrong_password_gets_nothing() {
    let world = plain_world().await;
    let mut browser = world.browser();
    let (code, body) = browser.login("not it").await;
    assert_eq!(code, 401, "{body}");
    assert!(browser.cookie.is_empty());
    assert_eq!(browser.get("/status").await.0, 401);
}

#[tokio::test]
async fn the_first_password_only_unlocks_setup() {
    let world = plain_world().await;
    let mut browser = world.signed_in(FIRST_PASSWORD).await;

    let (_, session) = browser.get("/session").await;
    assert_eq!(session["must_set_password"], true);

    let (code, body) = browser.get("/status").await;
    assert_eq!(code, 403, "{body}");
    assert_eq!(body["code"], "password_setup_required");

    let (code, body) = browser
        .post("/password", Some(json!({ "new": "short" })))
        .await;
    assert_eq!(code, 400, "{body}");
    let (code, body) = browser
        .post("/password", Some(json!({ "new": FIRST_PASSWORD })))
        .await;
    assert_eq!(code, 400, "the same password isn't a new one: {body}");

    let new = json!({ "new": REAL_PASSWORD });
    let (code, body) = browser.post("/password", Some(new)).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["must_set_password"], false);
    assert_eq!(browser.get("/status").await.0, 200);

    assert_eq!(world.browser().login(FIRST_PASSWORD).await.0, 401);
    assert_eq!(world.browser().login(REAL_PASSWORD).await.0, 200);
}

#[tokio::test]
async fn changing_the_password_signs_out_everyone_else() {
    let world = plain_world().await;
    let mut admin = world.admin().await;
    let mut other = world.signed_in(REAL_PASSWORD).await;
    assert_eq!(other.get("/status").await.0, 200);

    let body = json!({ "current": "wrong", "new": "another long password" });
    assert_eq!(admin.post("/password", Some(body)).await.0, 401);

    let body = json!({ "current": REAL_PASSWORD, "new": "another long password" });
    let (code, reply) = admin.post("/password", Some(body)).await;
    assert_eq!(code, 200, "{reply}");
    assert_eq!(admin.get("/status").await.0, 200, "this browser stays in");
    assert_eq!(other.get("/status").await.0, 401);
}

#[tokio::test]
async fn revoking_sessions_keeps_only_the_one_that_asked() {
    let world = plain_world().await;
    let mut admin = world.admin().await;
    let mut other = world.signed_in(REAL_PASSWORD).await;
    let (code, body) = admin.post("/sessions/revoke", None).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(admin.get("/status").await.0, 200);
    assert_eq!(other.get("/status").await.0, 401);
}

#[tokio::test]
async fn logging_out_clears_the_cookie() {
    let world = plain_world().await;
    let mut admin = world.admin().await;
    let (code, body) = admin.post("/logout", None).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(admin.cookie, format!("{SESSION_COOKIE}="));
    assert_eq!(admin.get("/status").await.0, 401);
}

#[tokio::test]
async fn a_forged_session_is_refused() {
    let world = plain_world().await;
    let mut browser = world.browser();
    browser.cookie = format!("{SESSION_COOKIE}=e30.AAAA");
    assert_eq!(browser.get("/status").await.0, 401);
}

#[tokio::test]
async fn writes_need_the_csrf_token_but_reads_do_not() {
    let world = plain_world().await;
    let mut admin = world.admin().await;
    let token = admin.csrf.clone();

    admin.csrf = "wrong".to_string();
    assert_eq!(admin.get("/status").await.0, 200);
    let (code, body) = admin.post("/markets/ITEM/halt", None).await;
    assert_eq!(code, 403, "{body}");
    assert_eq!(body["code"], "bad_csrf_token");

    admin.csrf = token;
    assert_eq!(admin.post("/markets/ITEM/halt", None).await.0, 200);
}

#[tokio::test]
async fn writes_from_another_site_are_refused() {
    let world = plain_world().await;
    let admin = world.admin().await;
    let response = world
        .http
        .post(format!("{}/admin/v1/markets/ITEM/halt", world.url))
        .header("cookie", &admin.cookie)
        .header(CSRF_HEADER, &admin.csrf)
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);

    let response = world
        .http
        .post(format!("{}/admin/v1/login", world.url))
        .header("origin", "https://evil.example")
        .json(&json!({ "password": REAL_PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
}

#[tokio::test]
async fn the_token_still_works_and_needs_no_csrf() {
    let world = plain_world().await;
    let (code, _) = world.call(Method::POST, "/markets/ITEM/halt", None).await;
    assert_eq!(code, 200);
    let response = world
        .http
        .get(format!("{}/admin/v1/status", world.url))
        .bearer_auth("wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(response.headers()["www-authenticate"], "Bearer");
}

#[tokio::test]
async fn repeated_failures_are_slowed_down_per_address() {
    let path = temp_markets(FILE);
    let gateway = spawn(fake_gateway(FakeGateway::new(&path))).await;
    let world = world_with(path, vec![gateway], quest()).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let service = server::router(world.state.clone())
        .into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, service).await });

    let mut browser = world.browser();
    browser.url = format!("http://{address}");
    for _ in 0..10 {
        assert_eq!(browser.login("not it").await.0, 401);
    }
    let (code, body) = browser.login(FIRST_PASSWORD).await;
    assert_eq!(code, 429, "even the right password waits: {body}");
}

#[tokio::test]
async fn insecure_local_refuses_browsers_without_a_session() {
    let path = temp_markets(FILE);
    let store = Arc::new(ConfigStore::Local(LocalStore {
        markets_file: path.clone(),
        dir: path.with_file_name("state"),
    }));
    let auth = Auth::start(store.clone(), None, true, None).await.unwrap();
    let targets = Targets {
        gateways: vec![dead_url().await],
        gateway_dns: None,
        outbox: dead_url().await,
        questdb: dead_url().await,
        nats: "127.0.0.1:1".to_string(),
        valkey: None,
    };
    let state = Arc::new(AppState {
        up: Arc::new(Upstreams::new(targets, None).unwrap()),
        store,
        auth,
        poll_every: Duration::from_millis(100),
        edits: tokio::sync::Mutex::new(()),
        behind_since: Mutex::new(None),
        outbox_restart: String::new(),
    });
    let url = spawn(server::router(state)).await;
    let http = reqwest::Client::new();
    let address = format!("{url}/admin/v1/markets/ITEM/halt");
    assert_eq!(http.post(&address).send().await.unwrap().status(), 502);
    let from_a_page = http
        .post(&address)
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(from_a_page.status(), 401);
}

#[tokio::test]
async fn the_token_can_reset_a_lost_password() {
    let world = plain_world().await;
    let mut admin = world.admin().await;
    assert_eq!(admin.post("/password/reset", None).await.0, 403);

    let (code, body) = world.call(Method::POST, "/password/reset", None).await;
    assert_eq!(code, 200, "{body}");
    let password = body["password"].as_str().unwrap();
    assert_eq!(admin.get("/status").await.0, 401);

    let mut browser = world.signed_in(password).await;
    let (_, session) = browser.get("/session").await;
    assert_eq!(session["must_set_password"], true);
}

#[tokio::test]
async fn secrets_never_come_back() {
    let world = plain_world().await;
    let mut admin = world.admin().await;
    let (_, secret) = admin.post("/webhooks/secret", None).await;
    let secret = secret["secret"].as_str().unwrap().to_string();
    assert_eq!(secret.len(), 64, "{secret}");
    admin
        .put(
            "/settings",
            json!({ "public_url": "http://localhost:8093" }),
        )
        .await;
    let oidc = json!({
        "enabled": false, "issuer": "", "client_id": "", "client_secret": "very-secret-value",
    });
    assert_eq!(admin.put("/settings/oidc", oidc).await.0, 200);

    for path in ["/settings", "/webhooks/config", "/session"] {
        let (_, body) = admin.get(path).await;
        let text = body.to_string();
        assert!(!text.contains(&secret), "{path}: {text}");
        assert!(!text.contains("very-secret-value"), "{path}: {text}");
        assert!(!text.contains("argon2"), "{path}: {text}");
    }
    let (_, settings) = admin.get("/settings").await;
    assert_eq!(settings["oidc"]["client_secret_hint"], "...alue");
}

#[tokio::test]
async fn the_console_is_a_page_and_the_api_is_not() {
    let world = plain_world().await;
    let response = world
        .http
        .get(format!("{}/admin/v1/nothing", world.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    let response = world
        .http
        .post(format!("{}/x", world.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 405);
}

#[tokio::test]
async fn the_editor_reads_and_saves_the_file_exactly() {
    let world = plain_world().await;
    let mut admin = world.admin().await;
    let (code, file) = admin.get("/config/markets").await;
    assert_eq!(code, 200, "{file}");
    assert_eq!(file["yaml"], FILE);
    assert_eq!(file["valid"], true);
    assert_eq!(file["backend"], "local");

    let yaml =
        format!("{FILE}  # a comment between entries\n  - symbol: GEM\n    tick_size: \"0.5\"\n");
    let body = json!({ "yaml": yaml, "revision": file["revision"] });
    let (code, reply) = admin.put("/config/markets", body).await;
    assert_eq!(code, 200, "{reply}");
    assert_eq!(reply["state"], "applied");
    assert_eq!(world.file_text(), yaml, "comments survive");

    let (_, markets) = admin.get("/markets").await;
    assert_eq!(markets["delivery"]["state"], "in_sync");
    assert_eq!(markets["markets"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn a_bad_file_is_refused_with_its_line_and_changes_nothing() {
    let world = plain_world().await;
    let mut admin = world.admin().await;
    let yaml = "markets:\n  - symbol: GEM\n    tick_size: \"0\"\n";

    let (code, reply) = admin
        .post("/config/markets/validate", Some(json!({ "yaml": yaml })))
        .await;
    assert_eq!(code, 200);
    assert_eq!(reply["valid"], false);
    assert_eq!(reply["problem"]["line"], 2);

    let (code, reply) = admin.put("/config/markets", json!({ "yaml": yaml })).await;
    assert_eq!(code, 422, "{reply}");
    assert!(
        reply["message"].as_str().unwrap().contains("Line 2"),
        "{reply}"
    );
    assert_eq!(world.file_text(), FILE);
}

#[tokio::test]
async fn validation_names_the_changes_and_what_stays_behind() {
    let world = plain_world().await;
    let mut admin = world.admin().await;
    let yaml = "markets:\n  - symbol: GEM\n    tick_size: \"0.5\"\n";
    let (_, reply) = admin
        .post("/config/markets/validate", Some(json!({ "yaml": yaml })))
        .await;
    assert_eq!(reply["valid"], true);
    assert_eq!(reply["changes"]["added"], json!(["GEM"]));
    assert_eq!(reply["changes"]["removed"], json!(["ITEM"]));
    assert!(
        reply["warnings"][0]
            .as_str()
            .unwrap()
            .contains("cancel them first")
    );
}

#[tokio::test]
async fn an_editor_holding_an_old_copy_can_not_overwrite_a_newer_one() {
    let world = plain_world().await;
    let mut admin = world.admin().await;
    let (_, file) = admin.get("/config/markets").await;
    fs::write(
        &world.file,
        format!("{FILE}  - symbol: NEW\n    tick_size: \"1\"\n"),
    )
    .unwrap();

    let body = json!({ "yaml": FILE, "revision": file["revision"] });
    let (code, reply) = admin.put("/config/markets", body).await;
    assert_eq!(code, 409, "{reply}");
    assert_eq!(reply["code"], "markets_changed");
    assert!(world.file_text().contains("NEW"));
}

#[tokio::test]
async fn a_broken_file_can_still_be_opened_and_fixed() {
    let world = plain_world().await;
    let mut admin = world.admin().await;
    fs::write(&world.file, "markets: [\n").unwrap();

    assert_eq!(admin.get("/markets").await.0, 500);
    let (code, file) = admin.get("/config/markets").await;
    assert_eq!(code, 200);
    assert_eq!(file["valid"], false);
    assert_eq!(file["yaml"], "markets: [\n");

    let body = json!({ "yaml": FILE, "revision": file["revision"] });
    assert_eq!(admin.put("/config/markets", body).await.0, 200);
    assert_eq!(admin.get("/markets").await.0, 200);
}

#[tokio::test]
async fn a_halt_stays_across_a_save_and_a_reload() {
    let world = plain_world().await;
    let mut admin = world.admin().await;
    assert_eq!(admin.post("/markets/ITEM/halt", None).await.0, 200);

    let (_, file) = admin.get("/config/markets").await;
    let yaml = format!("{FILE}  - symbol: GEM\n    tick_size: \"1\"\n");
    let body = json!({ "yaml": yaml, "revision": file["revision"] });
    assert_eq!(admin.put("/config/markets", body).await.0, 200);
    assert_eq!(admin.post("/markets/reload", None).await.0, 200);

    let (_, market) = admin.get("/markets/ITEM").await;
    assert_eq!(market["halted"], true);
}

#[tokio::test]
async fn a_hand_edit_shows_as_out_of_sync_until_a_reload() {
    let world = plain_world().await;
    let mut admin = world.admin().await;
    fs::write(
        &world.file,
        format!("{FILE}  - symbol: GEM\n    tick_size: \"1\"\n"),
    )
    .unwrap();

    let (_, markets) = admin.get("/markets").await;
    assert_eq!(markets["delivery"]["state"], "out_of_sync");
    assert_eq!(admin.post("/markets/reload", None).await.0, 200);
    let (_, markets) = admin.get("/markets").await;
    assert_eq!(markets["delivery"]["state"], "in_sync");
}

#[tokio::test]
async fn webhook_state_follows_the_outbox() {
    let world = plain_world().await;
    let mut admin = world.admin().await;
    let (code, view) = admin.get("/webhooks/config").await;
    assert_eq!(code, 200, "{view}");
    assert_eq!(view["state"], "incomplete");

    let (code, view) = admin
        .put("/webhooks/config", json!({ "url": "not a url" }))
        .await;
    assert_eq!(code, 400, "{view}");

    let url = "https://game.example/fills";
    let (_, view) = admin.put("/webhooks/config", json!({ "url": url })).await;
    assert_eq!(view["state"], "incomplete");
    let (_, view) = admin.post("/webhooks/secret", None).await;
    let secret = view["secret"].as_str().unwrap().to_string();
    assert_eq!(
        view["state"], "unknown",
        "this outbox can't say what it runs"
    );
    assert_eq!(view["restart"], "restart-the-outbox");

    let env = fs::read_to_string(world.dir.join("outbox.env")).unwrap();
    assert_eq!(env, format!("WEBHOOK_URL={url}\nWEBHOOK_SECRET={secret}\n"));

    let running = |id: &str| {
        format!(
            r#"{{"durable_position":1,"durable_exec_id":1,"delivered_position":1,"verified":true,"halted":null,"consecutive_failures":0,"webhook_url":"{url}","config_id":"{id}","configured":true}}"#
        )
    };
    *world.position.lock().unwrap() = running("something-else");
    assert_eq!(
        admin.get("/webhooks/config").await.1["state"],
        "restart_required"
    );
    *world.position.lock().unwrap() = running(&webhook_fingerprint(url, &secret));
    assert_eq!(admin.get("/webhooks/config").await.1["state"], "applied");
}

#[tokio::test]
async fn an_outbox_with_no_webhook_is_named_in_status() {
    let world = plain_world().await;
    *world.position.lock().unwrap() = r#"{"durable_position":0,"durable_exec_id":0,"delivered_position":0,"verified":true,"halted":null,"consecutive_failures":0,"configured":false}"#.to_string();
    let (_, status) = world.call(Method::GET, "/status", None).await;
    let outbox = status["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|service| service["name"] == "outbox")
        .unwrap();
    assert_eq!(outbox["error"], "No webhook is configured yet.");
    assert_eq!(
        world.call(Method::GET, "/webhooks", None).await.1["state"],
        "not_configured"
    );
}

#[tokio::test]
async fn gateways_can_be_found_through_a_name() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let targets = Targets {
        gateways: vec!["http://ignored".to_string()],
        gateway_dns: Some(format!("localhost:{port}")),
        outbox: String::new(),
        questdb: String::new(),
        nats: String::new(),
        valkey: None,
    };
    let up = Upstreams::new(targets, None).unwrap();
    let urls = up.gateway_urls().await;
    assert!(
        urls.contains(&format!("http://127.0.0.1:{port}")),
        "{urls:?}"
    );
    assert!(!urls.iter().any(|url| url.contains("ignored")), "{urls:?}");
}

// ---- A fake Kubernetes API ------------------------------------------------------------------

const NAMESPACE: &str = "mmx";
const CONFIG_MAP: &str = "mmx-config";
const SECRET: &str = "mmx-control";
const API_TOKEN: &str = "service-account-token";

#[derive(Default)]
struct FakeKube {
    objects: Mutex<HashMap<String, Value>>,
    version: Mutex<u64>,
    writes: Mutex<u64>,
}

impl FakeKube {
    fn new(config: Value) -> Arc<Self> {
        let kube = Self::default();
        let labels = json!({ "app.kubernetes.io/managed-by": "Helm" });
        let mut objects = kube.objects.lock().unwrap();
        objects.insert(
            format!("configmaps/{CONFIG_MAP}"),
            json!({ "metadata": { "name": CONFIG_MAP, "resourceVersion": "1", "labels": labels }, "data": config }),
        );
        objects.insert(
            format!("secrets/{SECRET}"),
            json!({ "metadata": { "name": SECRET, "resourceVersion": "2", "labels": labels }, "data": {} }),
        );
        drop(objects);
        *kube.version.lock().unwrap() = 2;
        Arc::new(kube)
    }

    /// What `kubectl apply` does.
    fn apply(&self, key: &str, value: &str) {
        let mut version = self.version.lock().unwrap();
        *version += 1;
        let mut objects = self.objects.lock().unwrap();
        let object = objects
            .get_mut(&format!("configmaps/{CONFIG_MAP}"))
            .unwrap();
        object["data"][key] = json!(value);
        object["metadata"]["resourceVersion"] = json!(version.to_string());
    }

    fn data(&self, path: &str) -> Value {
        self.objects.lock().unwrap()[path]["data"].clone()
    }
}

/// Serves only the two named objects, as the chart's RBAC does, and applies merge patches with the
/// API server's rules: a stale `resourceVersion` is a 409 and a null deletes a key.
async fn kube_route(State(kube): State<Arc<FakeKube>>, request: Request) -> Response {
    let refuse = |code: StatusCode, message: &str| {
        (code, Json(json!({ "kind": "Status", "message": message }))).into_response()
    };
    let (method, path) = (request.method().clone(), request.uri().path().to_string());
    let bearer = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok());
    if bearer != Some(&format!("Bearer {API_TOKEN}")) {
        return refuse(StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    let content_type = request
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let prefix = format!("/api/v1/namespaces/{NAMESPACE}/");
    let Some(key) = path.strip_prefix(&prefix).map(str::to_string) else {
        return refuse(StatusCode::FORBIDDEN, "forbidden: wrong namespace");
    };
    if !kube.objects.lock().unwrap().contains_key(&key) {
        return refuse(StatusCode::FORBIDDEN, "forbidden: not named in the Role");
    }
    match method {
        Method::GET => Json(kube.objects.lock().unwrap()[&key].clone()).into_response(),
        Method::PATCH => {
            if content_type != "application/merge-patch+json" {
                return refuse(StatusCode::UNSUPPORTED_MEDIA_TYPE, "wrong patch type");
            }
            let bytes = axum::body::to_bytes(request.into_body(), 1 << 20)
                .await
                .unwrap();
            let patch: Value = serde_json::from_slice(&bytes).unwrap();
            let mut version = kube.version.lock().unwrap();
            let mut objects = kube.objects.lock().unwrap();
            let object = objects.get_mut(&key).unwrap();
            if patch.pointer("/metadata/resourceVersion")
                != object.pointer("/metadata/resourceVersion")
            {
                return refuse(StatusCode::CONFLICT, "the object has been modified");
            }
            let data = object["data"].as_object_mut().unwrap();
            for (name, value) in patch["data"].as_object().unwrap() {
                if value.is_null() {
                    data.remove(name);
                } else {
                    data.insert(name.clone(), value.clone());
                }
            }
            *version += 1;
            *kube.writes.lock().unwrap() += 1;
            object["metadata"]["resourceVersion"] = json!(version.to_string());
            Json(object.clone()).into_response()
        }
        _ => refuse(StatusCode::METHOD_NOT_ALLOWED, "only get and patch"),
    }
}

async fn kube_store(kube: &Arc<FakeKube>) -> ConfigStore {
    let api = spawn(
        Router::new()
            .fallback(any(kube_route))
            .with_state(kube.clone()),
    )
    .await;
    let token_file = std::env::temp_dir().join(format!("mmx-token-{}", uuid::Uuid::new_v4()));
    fs::write(&token_file, format!("{API_TOKEN}\n")).unwrap();
    ConfigStore::Kube(KubeStore {
        api,
        namespace: NAMESPACE.to_string(),
        config_name: CONFIG_MAP.to_string(),
        secret_name: SECRET.to_string(),
        token_file: Some(token_file),
        http: reqwest::Client::new(),
    })
}

#[tokio::test]
async fn the_kubernetes_store_round_trips_and_leaves_labels_alone() {
    let kube = FakeKube::new(json!({ MARKETS_KEY: FILE }));
    let store = kube_store(&kube).await;

    let mut doc = store.config().await.unwrap();
    assert_eq!(doc.data[MARKETS_KEY], FILE);
    doc.data.insert("extra".to_string(), "1".to_string());
    let doc = store.put_config(&doc).await.unwrap();
    assert_eq!(doc.data["extra"], "1");

    let mut doc = doc;
    doc.data.remove("extra");
    store.put_config(&doc).await.unwrap();
    assert!(
        kube.data(&format!("configmaps/{CONFIG_MAP}"))
            .get("extra")
            .is_none()
    );
    let object = kube.objects.lock().unwrap()[&format!("configmaps/{CONFIG_MAP}")].clone();
    assert_eq!(
        object["metadata"]["labels"]["app.kubernetes.io/managed-by"],
        "Helm"
    );
}

#[tokio::test]
async fn kubernetes_secrets_are_base64_on_the_wire() {
    let kube = FakeKube::new(json!({ MARKETS_KEY: FILE }));
    let store = kube_store(&kube).await;
    let mut doc = store.secrets().await.unwrap();
    doc.data.insert("key".to_string(), "value".to_string());
    store.put_secrets(&doc).await.unwrap();

    let stored = kube.data(&format!("secrets/{SECRET}"));
    assert_eq!(stored["key"], STANDARD.encode("value"));
    assert_eq!(store.secrets().await.unwrap().data["key"], "value");
}

#[tokio::test]
async fn a_kubectl_apply_in_between_is_a_conflict_and_not_lost() {
    let kube = FakeKube::new(json!({ MARKETS_KEY: FILE }));
    let store = kube_store(&kube).await;
    let doc: Doc = store.config().await.unwrap();
    kube.apply("other", "kept");
    assert_eq!(store.put_config(&doc).await, Err(StoreError::Conflict));

    store
        .update_config(|data| {
            data.insert("mine".to_string(), "1".to_string());
            Ok(())
        })
        .await
        .unwrap();
    let stored = kube.data(&format!("configmaps/{CONFIG_MAP}"));
    assert_eq!(stored["other"], "kept");
    assert_eq!(stored["mine"], "1");
}

#[tokio::test]
async fn kubernetes_errors_say_what_to_look_at() {
    let kube = FakeKube::new(json!({ MARKETS_KEY: FILE }));
    let store = kube_store(&kube).await;
    let ConfigStore::Kube(mut inner) = store else {
        unreachable!()
    };
    inner.config_name = "someone-elses".to_string();
    let store = ConfigStore::Kube(inner);
    let message = store.config().await.unwrap_err().message();
    assert!(message.contains("ServiceAccount"), "{message}");
    assert!(message.contains("mmx/someone-elses"), "{message}");
}

#[tokio::test]
async fn replicas_starting_together_agree_on_one_password() {
    let kube = FakeKube::new(json!({ MARKETS_KEY: FILE }));
    let store = Arc::new(kube_store(&kube).await);
    let (first, second) = tokio::join!(
        Auth::start(store.clone(), Some(TOKEN), false, None),
        Auth::start(store.clone(), Some(TOKEN), false, None),
    );
    first.unwrap();
    second.unwrap();
    assert_eq!(*kube.writes.lock().unwrap(), 1);
}

/// A gateway whose file arrives only when the test says the kubelet delivered it.
async fn kube_world() -> (Arc<FakeKube>, Arc<FakeGateway>, PathBuf, World) {
    let mounted = temp_markets(FILE);
    let gateway = FakeGateway::new(&mounted);
    let url = spawn(fake_gateway(gateway.clone())).await;
    let kube = FakeKube::new(json!({ MARKETS_KEY: FILE }));
    let store = kube_store(&kube).await;
    let world = world_on(store, vec![url], quest()).await;
    (kube, gateway, mounted, world)
}

#[tokio::test]
async fn a_save_in_kubernetes_says_it_is_on_its_way() {
    let (kube, _gateway, mounted, world) = kube_world().await;
    let mut admin = world.admin().await;
    let (_, file) = admin.get("/config/markets").await;
    assert_eq!(file["backend"], "kubernetes");
    assert_eq!(file["delivery"], "eventual");

    let yaml = format!("{FILE}  - symbol: GEM\n    tick_size: \"1\"\n");
    let body = json!({ "yaml": yaml, "revision": file["revision"] });
    let (code, reply) = admin.put("/config/markets", body).await;
    assert_eq!(code, 202, "the gateway still has the old file: {reply}");
    assert_eq!(reply["state"], "reaching_gateways");
    assert_eq!(reply["applied"], false);
    assert_eq!(
        kube.data(&format!("configmaps/{CONFIG_MAP}"))[MARKETS_KEY],
        yaml.as_str()
    );

    let (_, markets) = admin.get("/markets").await;
    assert_eq!(markets["delivery"]["state"], "reaching_gateways");

    // The kubelet delivers it, and a reload then lands.
    fs::write(&mounted, &yaml).unwrap();
    let (code, reply) = admin.post("/markets/reload", None).await;
    assert_eq!(code, 200, "{reply}");
    let (_, markets) = admin.get("/markets").await;
    assert_eq!(markets["delivery"]["state"], "in_sync");
}

#[tokio::test]
async fn a_configmap_applied_by_hand_shows_up_and_reaches_gateways() {
    let (kube, _gateway, mounted, world) = kube_world().await;
    let mut admin = world.admin().await;

    let yaml = format!("{FILE}  - symbol: APPLY\n    tick_size: \"1\"\n");
    kube.apply(MARKETS_KEY, &yaml);
    let (_, file) = admin.get("/config/markets").await;
    assert_eq!(file["yaml"], yaml.as_str());
    let (_, markets) = admin.get("/markets").await;
    assert_eq!(markets["delivery"]["state"], "reaching_gateways");

    fs::write(&mounted, &yaml).unwrap();
    server::spawn_reconciler(world.state.clone());
    tokio::time::sleep(Duration::from_secs(6)).await;
    let (_, markets) = admin.get("/markets").await;
    assert_eq!(markets["delivery"]["state"], "in_sync", "{markets}");
}

#[tokio::test]
async fn a_bad_configmap_applied_by_hand_can_be_seen_and_fixed() {
    let (kube, _gateway, _mounted, world) = kube_world().await;
    let mut admin = world.admin().await;
    kube.apply(MARKETS_KEY, "markets: [\n");
    let (code, file) = admin.get("/config/markets").await;
    assert_eq!(code, 200);
    assert_eq!(file["valid"], false);
    assert!(file["problem"]["message"].is_string(), "{file}");
}

// ---- A fake OIDC issuer -----------------------------------------------------------------------

/// Made for these tests, and it signs nothing anywhere else.
const SIGNING_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgWtYj7oED23ylG4Tu
A8OeH3NqoFWwckfos5lWjbrYVq6hRANCAAShpVRK3IAGG2hcCBFuxX3BCE+IKUve
BfFtPz8vqdBl8k6U9WfZZq4dTM8f1oMIAMuP4x+KnjOxuJdm8QRH6TIC
-----END PRIVATE KEY-----
";
const KEY_X: &str = "oaVUStyABhtoXAgRbsV9wQhPiClL3gXxbT8_L6nQZfI";
const KEY_Y: &str = "TpT1Z9lmrh1Mzx_WgwgAy4_jH4qeM7G4l2bxBEfpMgI";
const CLIENT_ID: &str = "mmx-console";
const CLIENT_SECRET: &str = "client-secret-value";

#[derive(Serialize, Clone)]
struct IdClaims {
    iss: String,
    aud: String,
    sub: String,
    exp: u64,
    nonce: String,
    email: String,
    email_verified: bool,
}

#[derive(Default)]
struct Issuer {
    url: Mutex<String>,
    /// The challenge and nonce a browser arrived with, by code.
    logins: Mutex<HashMap<String, (String, String)>>,
    email: Mutex<String>,
    wrong_nonce: Mutex<bool>,
    expired: Mutex<bool>,
    wrong_audience: Mutex<bool>,
    wrong_key: Mutex<bool>,
    no_signature: Mutex<bool>,
    exchanges: Mutex<u32>,
}

fn sign_token(issuer: &Issuer, nonce: &str) -> String {
    let future = crate::tests::console::unix_now() + 300;
    let claims = IdClaims {
        iss: issuer.url.lock().unwrap().clone(),
        aud: if *issuer.wrong_audience.lock().unwrap() {
            "someone-else".to_string()
        } else {
            CLIENT_ID.to_string()
        },
        sub: "subject-1".to_string(),
        exp: if *issuer.expired.lock().unwrap() {
            1000
        } else {
            future
        },
        nonce: if *issuer.wrong_nonce.lock().unwrap() {
            "replayed".to_string()
        } else {
            nonce.to_string()
        },
        email: issuer.email.lock().unwrap().clone(),
        email_verified: true,
    };
    if *issuer.no_signature.lock().unwrap() {
        let head = URL_SAFE_NO_PAD.encode(r#"{"alg":"none","typ":"JWT"}"#);
        let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        return format!("{head}.{body}.");
    }
    if *issuer.wrong_key.lock().unwrap() {
        // HS256 with the client secret: the classic confusion attack.
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("key-1".to_string());
        let key = EncodingKey::from_secret(CLIENT_SECRET.as_bytes());
        return jsonwebtoken::encode(&header, &claims, &key).unwrap();
    }
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some("key-1".to_string());
    let key = EncodingKey::from_ec_pem(SIGNING_KEY.as_bytes()).unwrap();
    jsonwebtoken::encode(&header, &claims, &key).unwrap()
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

async fn discovery(State(issuer): State<Arc<Issuer>>) -> Json<Value> {
    let url = issuer.url.lock().unwrap().clone();
    Json(json!({
        "issuer": url,
        "authorization_endpoint": format!("{url}/authorize"),
        "token_endpoint": format!("{url}/token"),
        "jwks_uri": format!("{url}/jwks"),
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic"],
    }))
}

async fn jwks() -> Json<Value> {
    Json(json!({ "keys": [{
        "kty": "EC", "crv": "P-256", "alg": "ES256", "use": "sig", "kid": "key-1",
        "x": KEY_X, "y": KEY_Y,
    }] }))
}

/// Holds the client to the protocol: its secret, the redirect it registered, and a PKCE verifier
/// that matches the challenge sent through the browser.
async fn token(
    State(issuer): State<Arc<Issuer>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let refuse =
        |error: &str| (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response();
    *issuer.exchanges.lock().unwrap() += 1;
    let expected = format!(
        "Basic {}",
        STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"))
    );
    if headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        != Some(&expected)
    {
        return refuse("invalid_client");
    }
    if form.get("grant_type").map(String::as_str) != Some("authorization_code") {
        return refuse("unsupported_grant_type");
    }
    let code = form.get("code").cloned().unwrap_or_default();
    let Some((challenge, nonce)) = issuer.logins.lock().unwrap().remove(&code) else {
        return refuse("invalid_grant");
    };
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    if computed != challenge {
        return refuse("invalid_grant");
    }
    Json(json!({ "id_token": sign_token(&issuer, &nonce), "token_type": "Bearer" })).into_response()
}

async fn fake_issuer() -> Arc<Issuer> {
    let issuer = Arc::new(Issuer::default());
    *issuer.email.lock().unwrap() = "admin@game.example".to_string();
    let router = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", get(jwks))
        .route("/token", post(token))
        .with_state(issuer.clone());
    *issuer.url.lock().unwrap() = spawn(router).await;
    issuer
}

struct Login {
    world: World,
    issuer: Arc<Issuer>,
    admin: Browser,
}

async fn with_provider() -> Login {
    let issuer = fake_issuer().await;
    let world = plain_world().await;
    let mut admin = world.admin().await;
    let public = json!({ "public_url": world.url });
    assert_eq!(admin.put("/settings", public).await.0, 200);
    let oidc = json!({
        "enabled": true,
        "issuer": *issuer.url.lock().unwrap(),
        "client_id": CLIENT_ID,
        "client_secret": CLIENT_SECRET,
        "display_name": "Game SSO",
        "allowed": ["admin@game.example"],
    });
    let (code, body) = admin.put("/settings/oidc", oidc).await;
    assert_eq!(code, 200, "{body}");
    Login {
        world,
        issuer,
        admin,
    }
}

impl Login {
    /// Follows the browser's whole journey, standing in for the provider's login page.
    async fn sign_in(&self) -> (Browser, String) {
        let world = &self.world;
        let started = world
            .http
            .get(format!("{}/admin/v1/oidc/start", world.url))
            .send()
            .await
            .unwrap();
        assert_eq!(started.status(), 303);
        let location = started.headers()["location"].to_str().unwrap().to_string();
        let cookie = started.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax"),
            "{cookie}"
        );
        assert!(!location.contains(CLIENT_SECRET), "{location}");

        let url = url::Url::parse(&location).unwrap();
        let param = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_string())
                .unwrap_or_default()
        };
        assert_eq!(param("code_challenge_method"), "S256");
        assert_eq!(param("response_type"), "code");
        assert_eq!(
            param("redirect_uri"),
            format!("{}/admin/v1/oidc/callback", world.url)
        );
        self.issuer.logins.lock().unwrap().insert(
            "the-code".to_string(),
            (param("code_challenge"), param("nonce")),
        );

        let back = world
            .http
            .get(format!("{}/admin/v1/oidc/callback", world.url))
            .query(&[("code", "the-code".to_string()), ("state", param("state"))])
            .header("cookie", cookie_pair(&cookie))
            .send()
            .await
            .unwrap();
        let landed = back.headers()["location"].to_str().unwrap().to_string();
        let mut browser = world.browser();
        if let Some(header) = session_cookie(&back) {
            browser.cookie = cookie_pair(&header);
        }
        (browser, landed)
    }
}

#[tokio::test]
async fn a_full_sign_in_gives_a_session() {
    let login = with_provider().await;
    let (mut browser, landed) = login.sign_in().await;
    assert_eq!(landed, "/");
    let (_, session) = browser.get("/session").await;
    assert_eq!(session["authenticated"], true);
    assert_eq!(session["method"], "oidc");
    assert_eq!(session["user"], "admin@game.example");
    assert_eq!(browser.get("/status").await.0, 200);
}

#[tokio::test]
async fn the_login_page_can_offer_the_provider_by_name() {
    let login = with_provider().await;
    let mut anonymous = login.world.browser();
    let (_, session) = anonymous.get("/session").await;
    assert_eq!(session["oidc"]["label"], "Game SSO");
    assert_eq!(session["authenticated"], false);
}

async fn refused(setup: impl Fn(&Issuer)) -> String {
    let login = with_provider().await;
    setup(&login.issuer);
    let (mut browser, landed) = login.sign_in().await;
    assert!(landed.starts_with("/login?error="), "{landed}");
    assert!(browser.cookie.is_empty(), "{landed}");
    assert_eq!(browser.get("/status").await.0, 401);
    landed
}

#[tokio::test]
async fn someone_not_on_the_list_is_refused() {
    let landed = refused(|issuer| {
        *issuer.email.lock().unwrap() = "player@game.example".to_string();
    })
    .await;
    assert!(landed.contains("allowed"), "{landed}");
}

#[tokio::test]
async fn a_replayed_nonce_is_refused() {
    let landed = refused(|issuer| *issuer.wrong_nonce.lock().unwrap() = true).await;
    assert!(landed.contains("nonce"), "{landed}");
}

#[tokio::test]
async fn an_expired_token_is_refused() {
    refused(|issuer| *issuer.expired.lock().unwrap() = true).await;
}

#[tokio::test]
async fn a_token_for_another_client_is_refused() {
    refused(|issuer| *issuer.wrong_audience.lock().unwrap() = true).await;
}

#[tokio::test]
async fn a_token_signed_with_the_client_secret_is_refused() {
    let landed = refused(|issuer| *issuer.wrong_key.lock().unwrap() = true).await;
    assert!(landed.contains("allowed"), "{landed}");
}

#[tokio::test]
async fn an_unsigned_token_is_refused() {
    refused(|issuer| *issuer.no_signature.lock().unwrap() = true).await;
}

#[tokio::test]
async fn a_callback_with_the_wrong_state_never_reaches_the_provider() {
    let login = with_provider().await;
    let world = &login.world;
    let started = world
        .http
        .get(format!("{}/admin/v1/oidc/start", world.url))
        .send()
        .await
        .unwrap();
    let cookie = started.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .to_string();
    let back = world
        .http
        .get(format!("{}/admin/v1/oidc/callback", world.url))
        .query(&[("code", "x"), ("state", "forged")])
        .header("cookie", cookie_pair(&cookie))
        .send()
        .await
        .unwrap();
    let landed = back.headers()["location"].to_str().unwrap();
    assert!(landed.starts_with("/login?error="), "{landed}");
    assert_eq!(*login.issuer.exchanges.lock().unwrap(), 0);
}

#[tokio::test]
async fn a_callback_with_no_login_in_progress_is_refused() {
    let login = with_provider().await;
    let world = &login.world;
    let back = world
        .http
        .get(format!(
            "{}/admin/v1/oidc/callback?code=x&state=y",
            world.url
        ))
        .send()
        .await
        .unwrap();
    assert!(
        back.headers()["location"]
            .to_str()
            .unwrap()
            .contains("expired")
    );
}

#[tokio::test]
async fn passwords_can_only_be_turned_off_by_someone_who_used_the_provider() {
    let mut login = with_provider().await;
    let off = json!({ "password_login": false });
    let (code, body) = login.admin.put("/settings", off.clone()).await;
    assert_eq!(code, 400, "{body}");

    let (mut browser, _) = login.sign_in().await;
    browser.get("/session").await;
    let (code, body) = browser.put("/settings", off).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(login.world.browser().login(REAL_PASSWORD).await.0, 403);

    // The way back in, for when the provider breaks.
    let on = json!({ "password_login": true });
    let (code, _) = login.world.call(Method::PUT, "/settings", Some(on)).await;
    assert_eq!(code, 200);
    assert_eq!(login.world.browser().login(REAL_PASSWORD).await.0, 200);
}

#[tokio::test]
async fn the_provider_can_not_be_removed_while_it_is_the_only_way_in() {
    let login = with_provider().await;
    let (mut browser, _) = login.sign_in().await;
    browser.get("/session").await;
    browser
        .put("/settings", json!({ "password_login": false }))
        .await;
    let oidc = json!({ "enabled": false, "issuer": "", "client_id": "" });
    let (code, body) = browser.put("/settings/oidc", oidc).await;
    assert_eq!(code, 400, "{body}");
}

#[tokio::test]
async fn a_provider_needs_a_list_of_who_may_sign_in() {
    let login = with_provider().await;
    let mut admin = login.admin;
    let oidc = json!({
        "enabled": true,
        "issuer": *login.issuer.url.lock().unwrap(),
        "client_id": CLIENT_ID,
        "allowed": [],
    });
    let (code, body) = admin.put("/settings/oidc", oidc).await;
    assert_eq!(code, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("who may sign in")
    );
}

#[tokio::test]
async fn testing_a_provider_reports_what_was_found() {
    let login = with_provider().await;
    let mut admin = login.admin;
    let issuer = login.issuer.url.lock().unwrap().clone();
    let (_, reply) = admin
        .post("/settings/oidc/test", Some(json!({ "issuer": issuer })))
        .await;
    assert_eq!(reply["ok"], true, "{reply}");
    let dead = dead_url().await;
    let (_, reply) = admin
        .post("/settings/oidc/test", Some(json!({ "issuer": dead })))
        .await;
    assert_eq!(reply["ok"], false, "{reply}");
    assert!(reply["error"].is_string());
}

#[tokio::test]
async fn discovery_refuses_a_provider_with_another_name() {
    let issuer = fake_issuer().await;
    let address = issuer.url.lock().unwrap().clone();
    *issuer.url.lock().unwrap() = "https://other.example".to_string();
    let http = oidc::client().unwrap();
    let message = oidc::discover(&http, &address).await.unwrap_err();
    assert!(message.contains("calls itself"), "{message}");
}

#[test]
fn hints_never_hold_a_whole_short_secret_by_accident() {
    assert_eq!(auth::hint("abcd"), "...abcd");
}

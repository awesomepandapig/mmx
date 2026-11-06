//! The CLI's side of the admin API, and of the gateway's order API.
//!
//! Each command prints people's output or `--json`, and fails with a message so the process exits
//! non-zero. A partial result is still printed before the failure.

use crate::markets::normalize_symbol;
use crate::output::{self, line};
use crate::upstream::{Reply, http_client, send};

use std::time::Duration;

use reqwest::{Client, Method, RequestBuilder};
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

/// Longer than anything a server call can take, including a cancel-all or a failover.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(150);

const PLAYER_HEADER: &str = "x-player-id";

fn text<'a>(value: &'a Value, name: &str) -> &'a str {
    value.get(name).and_then(Value::as_str).unwrap_or("-")
}

fn flag(value: &Value, name: &str) -> bool {
    value.get(name).and_then(Value::as_bool).unwrap_or(false)
}

fn list<'a>(value: &'a Value, name: &str) -> &'a [Value] {
    value
        .get(name)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn count(value: &Value, name: &str) -> u64 {
    value.get(name).and_then(Value::as_u64).unwrap_or(0)
}

/// Prints a reply, and turns a failed one into an error. A body with per-gateway results is worth
/// showing even on failure, because it says which gateways took the action.
fn finish(json: bool, reply: &Reply, human: impl Fn(&Value) -> String) -> Result<(), String> {
    if json {
        line(&serde_json::to_string_pretty(&reply.body).map_err(|err| err.to_string())?);
    } else if reply.ok() || reply.body.get("gateways").is_some() {
        line(&human(&reply.body));
    }
    match (reply.ok(), reply.body.get("gateways")) {
        (true, _) => Ok(()),
        (false, Some(_)) => Err("not every gateway succeeded, as shown above".to_string()),
        (false, None) => Err(reply.error()),
    }
}

fn render_status(body: &Value) -> String {
    let mut lines = vec![format!("verdict: {}", text(body, "verdict"))];
    for service in list(body, "services") {
        let state = if flag(service, "ready") {
            "ready"
        } else if flag(service, "reachable") {
            "not ready"
        } else {
            "unreachable"
        };
        let version = service
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("-");
        let mut row = format!(
            "{:<8} {:<11} {:<8} {}",
            text(service, "name"),
            state,
            version,
            text(service, "target")
        );
        if let Some(error) = service.get("error").and_then(Value::as_str) {
            row = format!("{row}  ({error})");
        }
        lines.push(row);
    }
    lines.join("\n")
}

fn render_market(market: &Value) -> String {
    let state = match (flag(market, "enabled"), flag(market, "halted")) {
        (_, true) => "halted",
        (true, false) => "open",
        (false, false) => "disabled",
    };
    let sync = if flag(market, "in_sync") {
        "in sync"
    } else {
        "OUT OF SYNC"
    };
    format!(
        "{:<6} {state:<8} tick={} min={} max={} {sync}",
        text(market, "symbol"),
        text(market, "tick_size"),
        text(market, "min_size"),
        text(market, "max_size"),
    )
}

fn render_market_detail(market: &Value) -> String {
    let mut lines = vec![render_market(market)];
    for gateway in list(market, "gateways") {
        let state = match (flag(gateway, "reachable"), flag(gateway, "in_sync")) {
            (false, _) => format!("unreachable: {}", text(gateway, "error")),
            (true, true) => "in sync".to_string(),
            (true, false) => "out of sync".to_string(),
        };
        let halted = gateway
            .get("market")
            .is_some_and(|market| flag(market, "halted"));
        lines.push(format!(
            "  {} {state}{}",
            text(gateway, "gateway"),
            if halted { " halted" } else { "" }
        ));
    }
    lines.join("\n")
}

fn render_markets(body: &Value) -> String {
    let markets = list(body, "markets");
    if markets.is_empty() {
        return "no markets".to_string();
    }
    let lines: Vec<String> = markets.iter().map(render_market).collect();
    lines.join("\n")
}

fn render_gateways(body: &Value) -> String {
    let mut lines = Vec::new();
    for gateway in list(body, "gateways") {
        let state = if flag(gateway, "ok") {
            "ok"
        } else {
            text(gateway, "error")
        };
        lines.push(format!("  {} {state}", text(gateway, "gateway")));
    }
    lines.join("\n")
}

fn render_edit(body: &Value) -> String {
    let outcome = if flag(body, "applied") {
        "applied"
    } else if text(body, "state") == "reaching_gateways" {
        "saved, reaching gateways"
    } else {
        "written, but not applied everywhere"
    };
    let name = body.get("symbol").and_then(Value::as_str);
    let heading = name.map_or_else(|| outcome.to_string(), |name| format!("{name} {outcome}"));
    format!("{heading}\n{}", render_gateways(body))
}

fn render_webhook_config(body: &Value) -> String {
    let advice = match text(body, "state") {
        "applied" => "the outbox is using this".to_string(),
        "restart_required" => format!("saved. Restart the outbox: {}", text(body, "restart")),
        "from_environment" => "the outbox is using settings from its environment".to_string(),
        "incomplete" => "set both a URL and a secret".to_string(),
        _ => format!(
            "can't confirm what the outbox is using. Restart it to be sure: {}",
            text(body, "restart")
        ),
    };
    format!(
        "url: {}\nsecret: {}\n{advice}",
        text(body, "url"),
        text(body, "secret_hint")
    )
}

fn render_auth(body: &Value) -> String {
    let oidc = body.get("oidc").unwrap_or(&Value::Null);
    format!(
        "password login: {}\nsingle sign-on: {}\npublic url: {}",
        if flag(body, "password_login") {
            "on"
        } else {
            "off"
        },
        if flag(oidc, "enabled") {
            text(oidc, "issuer")
        } else {
            "off"
        },
        text(body, "public_url"),
    )
}

fn render_action(body: &Value) -> String {
    format!(
        "{} {}\n{}",
        text(body, "symbol"),
        text(body, "action"),
        render_gateways(body)
    )
}

fn render_cancel_all(body: &Value) -> String {
    let (requested, canceled, failed) = (
        count(body, "requested"),
        count(body, "canceled"),
        count(body, "failed"),
    );
    let mut summary = format!(
        "{}: {requested} requested, {canceled} canceled, {failed} failed",
        text(body, "symbol")
    );
    if flag(body, "truncated") {
        summary.push_str(", more remain, so run it again");
    }
    format!("{summary}\n{}", render_gateways(body))
}

fn render_activity(body: &Value, key: &str) -> String {
    let rows = list(body, key);
    if rows.is_empty() {
        return format!("no {key}");
    }
    let lines: Vec<String> = rows
        .iter()
        .map(|row| crate::watch::render(key.trim_end_matches('s'), row))
        .collect();
    lines.join("\n")
}

fn render_webhooks(body: &Value) -> String {
    crate::watch::render("webhook", body)
}

fn render_order(order: &Value) -> String {
    // An order with no record has only a status and what to do about it.
    if order.get("product_id").is_none() {
        return format!("{} {}: {}", text(order, "id"), text(order, "status"), text(order, "message"));
    }
    format!(
        "{} {:<6} {:<4} {} @ {} {} filled={}",
        text(order, "id"),
        text(order, "product_id"),
        text(order, "side"),
        text(order, "size"),
        order
            .get("price")
            .and_then(Value::as_str)
            .unwrap_or("market"),
        text(order, "status"),
        text(order, "filled_size"),
    )
}

fn render_orders(body: &Value) -> String {
    let orders = body.as_array().map_or(&[][..], Vec::as_slice);
    if orders.is_empty() {
        return "no orders".to_string();
    }
    let lines: Vec<String> = orders.iter().map(render_order).collect();
    lines.join("\n")
}

fn render_cancel(body: &Value) -> String {
    format!("{} {}", text(body, "id"), text(body, "status"))
}

/// Talks to `mmx serve`.
pub struct Admin {
    url: String,
    token: Option<String>,
    http: Client,
    json: bool,
}

/// Filters for `GET /orders` and `GET /trades`.
pub struct ActivityQuery {
    pub symbol: Option<String>,
    pub player: Option<String>,
    pub limit: Option<usize>,
}

impl ActivityQuery {
    fn pairs(&self) -> Vec<(&'static str, String)> {
        let mut pairs = Vec::new();
        pairs.extend(self.symbol.clone().map(|symbol| ("symbol", symbol)));
        pairs.extend(self.player.clone().map(|player| ("player", player)));
        pairs.extend(self.limit.map(|limit| ("limit", limit.to_string())));
        pairs
    }
}

impl Admin {
    pub fn new(url: String, token: Option<String>, json: bool) -> Result<Self, String> {
        Ok(Self {
            url,
            token,
            json,
            http: http_client(Some(CLIENT_TIMEOUT))?,
        })
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let request = self.http.request(method, format!("{}{path}", self.url));
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    async fn call(&self, request: RequestBuilder) -> Result<Reply, String> {
        send(request).await.map_err(|err| {
            format!(
                "can't reach mmx at {}: {err}. Is `mmx serve` running?",
                self.url
            )
        })
    }

    pub async fn status(&self) -> Result<(), String> {
        let reply = self
            .call(self.request(Method::GET, "/admin/v1/status"))
            .await?;
        finish(self.json, &reply, render_status)?;
        match text(&reply.body, "verdict") {
            "ok" => Ok(()),
            verdict => Err(format!("the exchange is {verdict}")),
        }
    }

    /// The server's own health, which needs no token.
    pub async fn health(&self) -> Result<(), String> {
        let alive = self.call(self.request(Method::GET, "/healthz")).await?;
        let ready = self.call(self.request(Method::GET, "/readyz")).await?;
        let body = json!({ "healthy": alive.ok(), "ready": ready.ok(), "checks": ready.body.get("checks") });
        if self.json {
            line(&body.to_string());
        } else {
            line(&format!("healthy: {}\nready: {}", alive.ok(), ready.ok()));
        }
        if alive.ok() && ready.ok() {
            Ok(())
        } else {
            Err(ready.error())
        }
    }

    pub async fn markets(&self) -> Result<(), String> {
        let reply = self
            .call(self.request(Method::GET, "/admin/v1/markets"))
            .await?;
        finish(self.json, &reply, render_markets)
    }

    pub async fn market(&self, symbol: &str) -> Result<(), String> {
        let path = format!("/admin/v1/markets/{}", normalize_symbol(symbol)?);
        let reply = self.call(self.request(Method::GET, &path)).await?;
        finish(self.json, &reply, render_market_detail)
    }

    pub async fn set_market(&self, symbol: &str, market: &Value) -> Result<(), String> {
        let path = format!("/admin/v1/markets/{}", normalize_symbol(symbol)?);
        let reply = self
            .call(self.request(Method::PUT, &path).json(market))
            .await?;
        finish(self.json, &reply, render_edit)
    }

    pub async fn remove_market(&self, symbol: &str) -> Result<(), String> {
        let path = format!("/admin/v1/markets/{}", normalize_symbol(symbol)?);
        let reply = self.call(self.request(Method::DELETE, &path)).await?;
        finish(self.json, &reply, render_edit)
    }

    pub async fn market_action(&self, symbol: &str, action: &str) -> Result<(), String> {
        let path = format!("/admin/v1/markets/{}/{action}", normalize_symbol(symbol)?);
        let reply = self.call(self.request(Method::POST, &path)).await?;
        finish(self.json, &reply, render_action)
    }

    pub async fn cancel_all(&self, symbol: &str) -> Result<(), String> {
        let path = format!("/admin/v1/markets/{}/cancel-all", normalize_symbol(symbol)?);
        let reply = self.call(self.request(Method::POST, &path)).await?;
        finish(self.json, &reply, render_cancel_all)?;
        if flag(&reply.body, "complete") {
            Ok(())
        } else {
            Err("some orders may still be open".to_string())
        }
    }

    pub async fn reload_markets(&self) -> Result<(), String> {
        let reply = self
            .call(self.request(Method::POST, "/admin/v1/markets/reload"))
            .await?;
        finish(self.json, &reply, render_edit)
    }

    async fn markets_file(&self) -> Result<Reply, String> {
        self.call(self.request(Method::GET, "/admin/v1/config/markets"))
            .await
    }

    /// Prints the file itself, so that the output can be saved and applied.
    pub async fn export_markets(&self) -> Result<(), String> {
        let reply = self.markets_file().await?;
        finish(self.json, &reply, |body| {
            text(body, "yaml").trim_end().to_string()
        })
    }

    pub async fn validate_markets(&self, yaml: &str) -> Result<(), String> {
        let request = self
            .request(Method::POST, "/admin/v1/config/markets/validate")
            .json(&json!({ "yaml": yaml }));
        let reply = self.call(request).await?;
        finish(self.json, &reply, |body| {
            if flag(body, "valid") {
                let mut lines = vec!["valid".to_string()];
                lines.extend(
                    list(body, "warnings")
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string),
                );
                lines.join("\n")
            } else {
                "not valid".to_string()
            }
        })?;
        if flag(&reply.body, "valid") {
            return Ok(());
        }
        let problem = reply.body.get("problem").unwrap_or(&Value::Null);
        let line = problem.get("line").and_then(Value::as_u64);
        Err(match line {
            Some(line) => format!("line {line}: {}", text(problem, "message")),
            None => text(problem, "message").to_string(),
        })
    }

    /// Sends no revision, so this replaces whatever is stored. A file named on the command line is
    /// meant as the whole truth, unlike an editor that may have opened an older copy.
    pub async fn apply_markets(&self, yaml: &str) -> Result<(), String> {
        let request = self
            .request(Method::PUT, "/admin/v1/config/markets")
            .json(&json!({ "yaml": yaml }));
        let reply = self.call(request).await?;
        finish(self.json, &reply, render_edit)
    }

    pub async fn show_auth(&self) -> Result<(), String> {
        let reply = self
            .call(self.request(Method::GET, "/admin/v1/settings"))
            .await?;
        finish(self.json, &reply, render_auth)
    }

    pub async fn reset_password(&self) -> Result<(), String> {
        let reply = self
            .call(self.request(Method::POST, "/admin/v1/password/reset"))
            .await?;
        finish(self.json, &reply, |body| {
            format!(
                "temporary password: {}\nIt's shown once. Sign in with it to choose a new one.",
                text(body, "password")
            )
        })
    }

    pub async fn set_password_login(&self, on: bool) -> Result<(), String> {
        let request = self
            .request(Method::PUT, "/admin/v1/settings")
            .json(&json!({ "password_login": on }));
        let reply = self.call(request).await?;
        finish(self.json, &reply, render_auth)
    }

    pub async fn revoke_sessions(&self) -> Result<(), String> {
        let reply = self
            .call(self.request(Method::POST, "/admin/v1/sessions/revoke"))
            .await?;
        finish(self.json, &reply, |_| {
            "every session was revoked".to_string()
        })
    }

    pub async fn webhook_config(&self) -> Result<(), String> {
        let reply = self
            .call(self.request(Method::GET, "/admin/v1/webhooks/config"))
            .await?;
        finish(self.json, &reply, render_webhook_config)
    }

    pub async fn set_webhook_url(&self, url: &str) -> Result<(), String> {
        let request = self
            .request(Method::PUT, "/admin/v1/webhooks/config")
            .json(&json!({ "url": url }));
        let reply = self.call(request).await?;
        finish(self.json, &reply, render_webhook_config)
    }

    pub async fn rotate_webhook_secret(&self) -> Result<(), String> {
        let reply = self
            .call(self.request(Method::POST, "/admin/v1/webhooks/secret"))
            .await?;
        finish(self.json, &reply, |body| {
            format!(
                "signing secret: {}\nIt's shown once.\n{}",
                text(body, "secret"),
                render_webhook_config(body)
            )
        })
    }

    pub async fn activity(&self, key: &str, query: &ActivityQuery) -> Result<(), String> {
        let path = format!("/admin/v1/{key}");
        let reply = self
            .call(self.request(Method::GET, &path).query(&query.pairs()))
            .await?;
        finish(self.json, &reply, |body| render_activity(body, key))
    }

    pub async fn webhooks(&self) -> Result<(), String> {
        let reply = self
            .call(self.request(Method::GET, "/admin/v1/webhooks"))
            .await?;
        finish(self.json, &reply, render_webhooks)
    }
}

/// What a game server sends. The gateway refuses unknown fields, so this is exactly its shape.
#[derive(Serialize, Debug)]
pub struct NewOrder {
    pub client_order_id: Uuid,
    pub product_id: String,
    pub side: String,
    pub r#type: String,
    pub size: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
}

/// Talks to a gateway directly, as a game server would.
pub struct Orders {
    gateway: String,
    player: Uuid,
    http: Client,
    json: bool,
}

/// A place that gets no clear answer may still have landed. The order must not be sent again, and
/// the ID is how to find out.
fn outcome_unknown(id: Uuid, reason: &str) -> String {
    format!(
        "the outcome of order {id} is unknown ({reason}). Don't send it again; check it with `mmx order get {id}`"
    )
}

impl Orders {
    pub fn new(gateway: String, player: Uuid, json: bool) -> Result<Self, String> {
        Ok(Self {
            gateway,
            player,
            json,
            http: http_client(Some(CLIENT_TIMEOUT))?,
        })
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.gateway))
            .header(PLAYER_HEADER, self.player.to_string())
    }

    /// Sends once. It never retries, because a retry after a lost answer could be a second order.
    pub async fn place(&self, order: &NewOrder) -> Result<(), String> {
        let id = order.client_order_id;
        output::note(&format!("client_order_id {id}"));
        let reply = send(self.request(Method::POST, "/api/v1/orders").json(order)).await;
        let reply = reply.map_err(|err| outcome_unknown(id, &err))?;
        // Other refusals say nothing was sent, so only this one leaves the order in doubt.
        if reply.body.get("code").and_then(Value::as_str) == Some("outcome_unknown") {
            finish(self.json, &reply, render_order).map_err(|err| outcome_unknown(id, &err))
        } else {
            finish(self.json, &reply, render_order)
        }
    }

    pub async fn cancel(&self, id: Uuid) -> Result<(), String> {
        let reply = self
            .call(self.request(Method::DELETE, &format!("/api/v1/orders/{id}")))
            .await?;
        finish(self.json, &reply, render_cancel)
    }

    pub async fn get(&self, id: Uuid) -> Result<(), String> {
        let reply = self
            .call(self.request(Method::GET, &format!("/api/v1/orders/{id}")))
            .await?;
        finish(self.json, &reply, render_order)
    }

    pub async fn list(
        &self,
        symbol: Option<&str>,
        status: Option<&str>,
        limit: Option<usize>,
    ) -> Result<(), String> {
        let mut query: Vec<(&str, String)> = Vec::new();
        query.extend(symbol.map(|symbol| ("product_id", symbol.to_string())));
        query.extend(status.map(|status| ("status", status.to_string())));
        query.extend(limit.map(|limit| ("limit", limit.to_string())));
        let reply = self
            .call(self.request(Method::GET, "/api/v1/orders").query(&query))
            .await?;
        finish(self.json, &reply, render_orders)
    }

    async fn call(&self, request: RequestBuilder) -> Result<Reply, String> {
        send(request)
            .await
            .map_err(|err| format!("can't reach the gateway at {}: {err}", self.gateway))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_order_with_no_record_shows_its_status_and_what_to_do() {
        let body = json!({ "id": "x", "status": "unknown", "settled": false, "message": "Keep funds on hold." });
        assert_eq!(render_order(&body), "x unknown: Keep funds on hold.");
    }

    #[test]
    fn status_shows_each_service_and_why_it_failed() {
        let body = json!({
            "verdict": "degraded",
            "services": [
                { "name": "gateway", "target": "http://g", "ready": true, "reachable": true, "version": "1.2" },
                { "name": "outbox", "target": "http://o", "ready": false, "reachable": true, "error": "halted" },
                { "name": "questdb", "target": "http://q", "ready": false, "reachable": false, "error": "refused" },
            ],
        });
        let text = render_status(&body);
        assert!(text.starts_with("verdict: degraded"), "{text}");
        assert!(text.contains("gateway  ready       1.2"), "{text}");
        assert!(
            text.contains("not ready") && text.contains("(halted)"),
            "{text}"
        );
        assert!(
            text.contains("unreachable") && text.contains("(refused)"),
            "{text}"
        );
    }

    #[test]
    fn a_partly_halted_market_reads_as_halted_and_out_of_sync() {
        let market = json!({ "symbol": "ITEM", "tick_size": "1", "enabled": true, "halted": true, "in_sync": false });
        let text = render_market(&market);
        assert!(
            text.contains("halted") && text.contains("OUT OF SYNC"),
            "{text}"
        );
    }

    #[test]
    fn a_market_order_shows_no_price() {
        let order = json!({ "id": "x", "product_id": "ITEM", "side": "buy", "size": "1", "status": "open", "filled_size": "0" });
        assert!(render_order(&order).contains("@ market"));
    }

    #[test]
    fn a_new_order_has_the_gateway_shape() {
        let order = NewOrder {
            client_order_id: Uuid::nil(),
            product_id: "ITEM".to_string(),
            side: "buy".to_string(),
            r#type: "market".to_string(),
            size: "1".to_string(),
            price: None,
        };
        let expected = json!({
            "client_order_id": Uuid::nil(),
            "product_id": "ITEM",
            "side": "buy",
            "type": "market",
            "size": "1",
        });
        assert_eq!(serde_json::to_value(&order).unwrap(), expected);
    }
}

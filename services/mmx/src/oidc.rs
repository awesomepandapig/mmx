//! OpenID Connect for console login: authorization code with PKCE, against one provider.
//!
//! The server holds the client secret and talks to the token endpoint itself, so the browser only
//! ever carries the code. The ID token is still checked in full, because a session is only worth
//! what its claims are.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use reqwest::{Client, redirect};
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;
use url::Url;

const TIMEOUT: Duration = Duration::from_secs(10);
const DISCOVERY_PATH: &str = "/.well-known/openid-configuration";
const SCOPES: &str = "openid email profile";
/// Asked for only if the provider lists it, because some refuse a scope they don't know.
const GROUPS_SCOPE: &str = "groups";

/// Only asymmetric algorithms: HS256 would turn the client secret into a signing key.
const ALGORITHMS: &[Algorithm] = &[
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

/// TLS is on here, unlike every other client in `mmx`, because providers are outside the exchange.
pub fn client() -> Result<Client, String> {
    crate::upstream::install_tls();
    Client::builder()
        .no_proxy()
        .redirect(redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()
        .map_err(|err| err.to_string())
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Vec<String>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

/// A provider must be HTTPS. Plain HTTP on loopback is allowed so it can be tested locally.
pub fn check_url(name: &str, text: &str) -> Result<Url, String> {
    let url = Url::parse(text).map_err(|err| format!("{name} isn't a URL: {err}"))?;
    let loopback = match url.host() {
        Some(url::Host::Domain(host)) => host == "localhost",
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    match url.scheme() {
        "https" => Ok(url),
        "http" if loopback => Ok(url),
        _ => Err(format!(
            "{name} must use https, unless it's on localhost: {text}"
        )),
    }
}

pub async fn discover(http: &Client, issuer: &str) -> Result<Provider, String> {
    check_url("issuer", issuer)?;
    let address = format!("{}{DISCOVERY_PATH}", issuer.trim_end_matches('/'));
    let response = http
        .get(&address)
        .send()
        .await
        .map_err(|err| format!("{address}: {}", crate::upstream::describe(&err)))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("{address}: HTTP {status}"));
    }
    let provider: Provider = response
        .json()
        .await
        .map_err(|err| format!("{address}: not a discovery document: {err}"))?;

    if provider.issuer.trim_end_matches('/') != issuer.trim_end_matches('/') {
        return Err(format!(
            "the provider calls itself {}, not {issuer}",
            provider.issuer
        ));
    }
    check_url("authorization_endpoint", &provider.authorization_endpoint)?;
    check_url("token_endpoint", &provider.token_endpoint)?;
    check_url("jwks_uri", &provider.jwks_uri)?;
    let methods = &provider.code_challenge_methods_supported;
    if !methods.is_empty() && !methods.iter().any(|method| method == "S256") {
        return Err("the provider doesn't support PKCE with S256".to_string());
    }
    Ok(provider)
}

pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// What the browser is sent to.
pub struct Request<'a> {
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub state: &'a str,
    pub nonce: &'a str,
    pub verifier: &'a str,
    /// Set for a confirmation: the person must sign in again, whatever session the provider holds.
    pub max_age: Option<u64>,
}

pub fn authorize_url(provider: &Provider, request: &Request) -> Result<String, String> {
    let mut url = Url::parse(&provider.authorization_endpoint).map_err(|err| err.to_string())?;
    let scopes = if provider
        .scopes_supported
        .iter()
        .any(|scope| scope == GROUPS_SCOPE)
    {
        format!("{SCOPES} {GROUPS_SCOPE}")
    } else {
        SCOPES.to_string()
    };
    if let Some(max_age) = request.max_age {
        url.query_pairs_mut()
            .append_pair("prompt", "login")
            .append_pair("max_age", &max_age.to_string());
    }
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", request.client_id)
        .append_pair("redirect_uri", request.redirect_uri)
        .append_pair("scope", &scopes)
        .append_pair("state", request.state)
        .append_pair("nonce", request.nonce)
        .append_pair("code_challenge", &pkce_challenge(request.verifier))
        .append_pair("code_challenge_method", "S256");
    Ok(url.into())
}

#[derive(Deserialize)]
struct TokenReply {
    id_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

pub struct Exchange<'a> {
    pub client_id: &'a str,
    pub client_secret: &'a str,
    pub code: &'a str,
    pub redirect_uri: &'a str,
    pub verifier: &'a str,
}

/// Swaps the code for an ID token. Basic auth is the spec's default, so the secret only goes in the
/// form if the provider says that's all it takes.
pub async fn exchange(
    http: &Client,
    provider: &Provider,
    exchange: &Exchange<'_>,
) -> Result<String, String> {
    let methods = &provider.token_endpoint_auth_methods_supported;
    let in_form = !methods.is_empty()
        && !methods.iter().any(|method| method == "client_secret_basic")
        && methods.iter().any(|method| method == "client_secret_post");

    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", exchange.code),
        ("redirect_uri", exchange.redirect_uri),
        ("client_id", exchange.client_id),
        ("code_verifier", exchange.verifier),
    ];
    if in_form {
        form.push(("client_secret", exchange.client_secret));
    }
    let mut request = http.post(&provider.token_endpoint).form(&form);
    if !in_form {
        request = request.basic_auth(exchange.client_id, Some(exchange.client_secret));
    }
    let response = request
        .send()
        .await
        .map_err(|err| format!("token endpoint: {}", crate::upstream::describe(&err)))?;
    let status = response.status();
    let reply: TokenReply = response
        .json()
        .await
        .map_err(|err| format!("token endpoint: HTTP {status}: unexpected answer: {err}"))?;
    if let Some(error) = reply.error {
        let detail = reply.error_description.unwrap_or_default();
        return Err(format!("token endpoint refused: {error} {detail}")
            .trim()
            .to_string());
    }
    reply
        .id_token
        .ok_or_else(|| format!("token endpoint: HTTP {status}: no ID token in the answer"))
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct Claims {
    pub sub: String,
    pub email: Option<String>,
    pub nonce: Option<String>,
    /// Some providers send this as a float.
    #[serde(default)]
    auth_time: Option<Value>,
    /// Everything else, because the groups claim's name is configurable.
    #[serde(flatten)]
    rest: Map<String, Value>,
}

impl Claims {
    /// What the console calls this person.
    pub fn display(&self) -> String {
        self.email.clone().unwrap_or_else(|| self.sub.clone())
    }

    pub fn auth_time(&self) -> Option<u64> {
        let time = self.auth_time.as_ref()?;
        time.as_u64()
            .or_else(|| time.as_f64().map(|time| time as u64))
    }

    /// The person's groups. Past a limit Entra leaves them out of the token and points at Graph
    /// instead, which would read as having no groups, so that's refused by name.
    pub fn groups(&self, claim: &str) -> Result<Vec<String>, String> {
        let overage = self
            .rest
            .get("_claim_names")
            .is_some_and(|names| names.get(claim).is_some());
        if overage {
            return Err(format!(
                "The provider left the {claim} claim out of the token, because this person is in \
                 too many groups. Have it send only the groups assigned to this application."
            ));
        }
        Ok(match self.rest.get(claim) {
            Some(Value::Array(groups)) => groups
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            Some(Value::String(group)) => vec![group.clone()],
            _ => Vec::new(),
        })
    }
}

/// Checks the signature, issuer, audience, expiry and nonce of an ID token.
pub async fn verify(
    http: &Client,
    provider: &Provider,
    client_id: &str,
    id_token: &str,
    nonce: &str,
) -> Result<Claims, String> {
    let header = decode_header(id_token).map_err(|err| format!("bad ID token: {err}"))?;
    if !ALGORITHMS.contains(&header.alg) {
        return Err(format!(
            "ID token uses {:?}, which isn't allowed",
            header.alg
        ));
    }

    let response = http
        .get(&provider.jwks_uri)
        .send()
        .await
        .map_err(|err| format!("jwks_uri: {}", crate::upstream::describe(&err)))?;
    let keys: JwkSet = response
        .json()
        .await
        .map_err(|err| format!("jwks_uri: not a key set: {err}"))?;
    let key = match &header.kid {
        Some(kid) => keys.find(kid),
        None if keys.keys.len() == 1 => keys.keys.first(),
        None => None,
    }
    .ok_or("no signing key matches the ID token")?;
    let key = DecodingKey::from_jwk(key).map_err(|err| format!("bad signing key: {err}"))?;

    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[&provider.issuer]);
    validation.set_audience(&[client_id]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    let claims = decode::<Claims>(id_token, &key, &validation)
        .map_err(|err| format!("ID token refused: {err}"))?
        .claims;

    let matches = claims
        .nonce
        .as_deref()
        .is_some_and(|sent| crate::auth::same(sent, nonce));
    if !matches {
        return Err("ID token refused: wrong nonce".to_string());
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claims(value: Value) -> Claims {
        let mut fields = json!({ "sub": "subject-1" });
        fields
            .as_object_mut()
            .unwrap()
            .extend(value.as_object().unwrap().clone());
        serde_json::from_value(fields).unwrap()
    }

    fn provider(scopes: &[&str]) -> Provider {
        Provider {
            issuer: "https://id.example".to_string(),
            authorization_endpoint: "https://id.example/auth".to_string(),
            token_endpoint: "https://id.example/token".to_string(),
            jwks_uri: "https://id.example/jwks".to_string(),
            token_endpoint_auth_methods_supported: Vec::new(),
            code_challenge_methods_supported: Vec::new(),
            scopes_supported: scopes.iter().map(ToString::to_string).collect(),
        }
    }

    fn request(max_age: Option<u64>) -> Request<'static> {
        Request {
            client_id: "mmx",
            redirect_uri: "http://mmx/admin/v1/oidc/callback",
            state: "s",
            nonce: "n",
            verifier: "v",
            max_age,
        }
    }

    #[test]
    fn urls_must_be_https_unless_local() {
        assert!(check_url("issuer", "https://id.example").is_ok());
        assert!(check_url("issuer", "http://localhost:9").is_ok());
        assert!(check_url("issuer", "http://127.0.0.1:9").is_ok());
        assert!(check_url("issuer", "http://id.example").is_err());
        assert!(check_url("issuer", "http://localhost.example").is_err());
        assert!(check_url("issuer", "not a url").is_err());
    }

    #[test]
    fn groups_come_from_the_configured_claim() {
        let person = claims(json!({ "groups": ["a", "b"], "roles": "c" }));
        assert_eq!(person.groups("groups").unwrap(), ["a", "b"]);
        assert_eq!(person.groups("roles").unwrap(), ["c"]);
        assert!(person.groups("other").unwrap().is_empty());
    }

    #[test]
    fn an_omitted_groups_claim_fails_by_name() {
        let person = claims(json!({
            "_claim_names": { "groups": "src1" },
            "_claim_sources": { "src1": { "endpoint": "https://graph.example" } },
        }));
        let message = person.groups("groups").unwrap_err();
        assert!(message.contains("too many groups"), "{message}");
    }

    #[test]
    fn auth_time_can_be_a_whole_number_or_a_float() {
        assert_eq!(claims(json!({ "auth_time": 1700 })).auth_time(), Some(1700));
        assert_eq!(
            claims(json!({ "auth_time": 1700.5 })).auth_time(),
            Some(1700)
        );
        assert_eq!(claims(json!({})).auth_time(), None);
    }

    #[test]
    fn the_groups_scope_is_only_asked_for_if_offered() {
        let plain = authorize_url(&provider(&["openid"]), &request(None)).unwrap();
        assert!(plain.contains("scope=openid+email+profile&"), "{plain}");
        let offered = authorize_url(&provider(&["openid", "groups"]), &request(None)).unwrap();
        assert!(offered.contains("profile+groups"), "{offered}");
    }

    #[test]
    fn a_confirmation_forces_a_new_sign_in() {
        let plain = authorize_url(&provider(&[]), &request(None)).unwrap();
        assert!(!plain.contains("prompt="), "{plain}");
        let fresh = authorize_url(&provider(&[]), &request(Some(300))).unwrap();
        assert!(
            fresh.contains("prompt=login") && fresh.contains("max_age=300"),
            "{fresh}"
        );
    }

    #[test]
    fn the_pkce_challenge_matches_the_rfc_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}

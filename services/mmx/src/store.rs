//! Where `mmx serve` keeps what operators change: one set of plain settings and one set of secrets.
//!
//! Locally these are files, with the markets file gateways read beside a state directory. In
//! Kubernetes they're one named ConfigMap and one named Secret, reached through the API with a
//! ServiceAccount that can touch nothing else. Nothing else is stored, because everything about the
//! exchange itself is derived from the tape.

use crate::markets;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::{Client, StatusCode, redirect};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

pub const MARKETS_KEY: &str = "markets.yaml";
pub const SETTINGS_KEY: &str = "console.yaml";
pub const WEBHOOK_URL_KEY: &str = "WEBHOOK_URL";
pub const WEBHOOK_SECRET_KEY: &str = "WEBHOOK_SECRET";

const CONFIG_FILE: &str = "config.json";
const SECRETS_FILE: &str = "secrets.json";
const OUTBOX_FILE: &str = "outbox.env";
const SERVICE_ACCOUNT: &str = "/var/run/secrets/kubernetes.io/serviceaccount";
const API_TIMEOUT: Duration = Duration::from_secs(5);
const UPDATE_ATTEMPTS: usize = 5;
const MISSING_KEY: &str = "something else keeps changing it";

pub type Data = BTreeMap<String, String>;

/// One read. `revision` guards the write that follows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Doc {
    pub data: Data,
    pub revision: String,
    /// What was there when read, because a merge patch needs to be told what to delete.
    read_keys: BTreeSet<String>,
}

impl Doc {
    fn new(data: Data, revision: String) -> Self {
        let read_keys = data.keys().cloned().collect();
        Self {
            data,
            revision,
            read_keys,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    /// Something else wrote first, so read again.
    Conflict,
    Failed(String),
}

impl StoreError {
    pub fn message(&self) -> String {
        match self {
            Self::Conflict => "Something else changed this first.".to_string(),
            Self::Failed(message) => message.clone(),
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(err: io::Error) -> Self {
        Self::Failed(err.to_string())
    }
}

/// How a write reaches gateways. In Kubernetes the kubelet delivers a ConfigMap to each pod on its
/// own schedule, so a reload straight after a write usually reads the old file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Immediate,
    Eventual,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreInfo {
    pub backend: &'static str,
    pub config: String,
    pub secrets: String,
    pub delivery: Delivery,
}

fn revision_of(data: &Data) -> String {
    let text = serde_json::to_string(data).unwrap_or_default();
    let hash = Sha256::digest(text.as_bytes());
    hash[..8].iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A revision for one value, for a client to hand back. It's separate from the document's, so that
/// saving a setting can't look like someone else edited the markets.
pub fn text_revision(text: &str) -> String {
    let hash = Sha256::digest(text.as_bytes());
    hash[..8].iter().map(|byte| format!("{byte:02x}")).collect()
}

fn read_map(path: &Path) -> Result<Data, StoreError> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|err| StoreError::Failed(format!("{}: {err}", path.display()))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Data::new()),
        Err(err) => Err(StoreError::Failed(format!("{}: {err}", path.display()))),
    }
}

/// Secrets must never be world-readable, even for a moment, so the mode is set before the rename.
#[cfg(unix)]
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    fs::write(&temporary, "")?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    fs::write(&temporary, text)?;
    fs::rename(&temporary, path)
}

#[cfg(not(unix))]
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    fs::write(path, text)
}

/// The settings files for a local stack. `mmx serve` must be the only writer, and it's the
/// server's `edits` lock that keeps writes in order.
pub struct LocalStore {
    pub markets_file: PathBuf,
    pub dir: PathBuf,
}

impl LocalStore {
    fn read_config(&self) -> Result<Doc, StoreError> {
        let mut data = read_map(&self.dir.join(CONFIG_FILE))?;
        let path = &self.markets_file;
        let markets = fs::read_to_string(path)
            .map_err(|err| StoreError::Failed(format!("{}: {err}", path.display())))?;
        data.insert(MARKETS_KEY.to_string(), markets);
        let revision = revision_of(&data);
        Ok(Doc::new(data, revision))
    }

    fn read_secrets(&self) -> Result<Doc, StoreError> {
        let data = read_map(&self.dir.join(SECRETS_FILE))?;
        let revision = revision_of(&data);
        Ok(Doc::new(data, revision))
    }

    /// What `mmx.py` gives the outbox on its next start, because the outbox only reads its
    /// environment. It's written whole, so a half-configured webhook can't look complete.
    fn write_outbox_env(&self, config: &Data, secrets: &Data) -> io::Result<()> {
        let (Some(url), Some(secret)) =
            (config.get(WEBHOOK_URL_KEY), secrets.get(WEBHOOK_SECRET_KEY))
        else {
            return Ok(());
        };
        let text = format!("{WEBHOOK_URL_KEY}={url}\n{WEBHOOK_SECRET_KEY}={secret}\n");
        write_private(&self.dir.join(OUTBOX_FILE), &text)
    }

    fn put_config(&self, doc: &Doc) -> Result<Doc, StoreError> {
        let current = self.read_config()?;
        if current.revision != doc.revision {
            return Err(StoreError::Conflict);
        }
        fs::create_dir_all(&self.dir)?;
        let mut data = doc.data.clone();
        if let Some(text) = data.remove(MARKETS_KEY)
            && current.data.get(MARKETS_KEY) != Some(&text)
        {
            markets::replace(&self.markets_file, &text).map_err(StoreError::Failed)?;
        }
        let text = serde_json::to_string_pretty(&data)
            .map_err(|err| StoreError::Failed(format!("can't serialize the settings: {err}")))?;
        write_private(&self.dir.join(CONFIG_FILE), &text)?;
        self.write_outbox_env(&data, &self.read_secrets()?.data)?;
        self.read_config()
    }

    fn put_secrets(&self, doc: &Doc) -> Result<Doc, StoreError> {
        if self.read_secrets()?.revision != doc.revision {
            return Err(StoreError::Conflict);
        }
        fs::create_dir_all(&self.dir)?;
        let text = serde_json::to_string_pretty(&doc.data)
            .map_err(|err| StoreError::Failed(format!("can't serialize the secrets: {err}")))?;
        write_private(&self.dir.join(SECRETS_FILE), &text)?;
        self.write_outbox_env(&self.read_config()?.data, &doc.data)?;
        self.read_secrets()
    }
}

#[derive(Clone, Copy)]
enum Kind {
    ConfigMap,
    Secret,
}

impl Kind {
    fn path(self) -> &'static str {
        match self {
            Self::ConfigMap => "configmaps",
            Self::Secret => "secrets",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::ConfigMap => "ConfigMap",
            Self::Secret => "Secret",
        }
    }
}

/// The Kubernetes API, for exactly one ConfigMap and one Secret. A write is a merge patch holding
/// the `resourceVersion` it read, so a `kubectl apply` in between is a conflict and not a silent
/// overwrite, and labels the chart set are left alone.
pub struct KubeStore {
    pub api: String,
    pub namespace: String,
    pub config_name: String,
    pub secret_name: String,
    /// Read for each call, because the kubelet rotates projected tokens.
    pub token_file: Option<PathBuf>,
    pub http: Client,
}

impl KubeStore {
    /// Inside a pod, from the service's environment and the mounted ServiceAccount.
    pub fn in_cluster(config_name: String, secret_name: String) -> Result<Self, String> {
        crate::upstream::install_tls();
        let host = std::env::var("KUBERNETES_SERVICE_HOST")
            .map_err(|_| "KUBERNETES_SERVICE_HOST isn't set, so this isn't a pod".to_string())?;
        let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".to_string());
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host
        };
        let account = Path::new(SERVICE_ACCOUNT);
        let namespace = fs::read_to_string(account.join("namespace"))
            .map_err(|err| format!("can't read the namespace: {err}"))?;
        let authority = fs::read(account.join("ca.crt"))
            .map_err(|err| format!("can't read the cluster CA: {err}"))?;
        let authority = reqwest::Certificate::from_pem(&authority)
            .map_err(|err| format!("bad cluster CA: {err}"))?;
        let http = Client::builder()
            .no_proxy()
            .redirect(redirect::Policy::none())
            .timeout(API_TIMEOUT)
            .tls_certs_only([authority])
            .build()
            .map_err(|err| err.to_string())?;
        Ok(Self {
            api: format!("https://{host}:{port}"),
            namespace: namespace.trim().to_string(),
            config_name,
            secret_name,
            token_file: Some(account.join("token")),
            http,
        })
    }

    fn url(&self, kind: Kind) -> String {
        let name = match kind {
            Kind::ConfigMap => &self.config_name,
            Kind::Secret => &self.secret_name,
        };
        format!(
            "{}/api/v1/namespaces/{}/{}/{name}",
            self.api,
            self.namespace,
            kind.path()
        )
    }

    fn describe(&self, kind: Kind) -> String {
        let name = match kind {
            Kind::ConfigMap => &self.config_name,
            Kind::Secret => &self.secret_name,
        };
        format!("{} {}/{name}", kind.name(), self.namespace)
    }

    fn authorized(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, StoreError> {
        let Some(path) = &self.token_file else {
            return Ok(request);
        };
        let token = fs::read_to_string(path).map_err(|err| {
            StoreError::Failed(format!("can't read the ServiceAccount token: {err}"))
        })?;
        Ok(request.bearer_auth(token.trim()))
    }

    fn refused(&self, kind: Kind, status: StatusCode, body: &str) -> StoreError {
        let what = self.describe(kind);
        let message = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|value| value.get("message")?.as_str().map(str::to_string))
            .unwrap_or_else(|| body.chars().take(200).collect());
        StoreError::Failed(match status {
            StatusCode::NOT_FOUND => format!("{what} doesn't exist. The Helm chart creates it."),
            StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => {
                format!("The ServiceAccount can't reach {what}: {message}")
            }
            _ => format!("{what}: HTTP {status}: {message}"),
        })
    }

    fn decode(kind: Kind, body: &Value) -> Result<Doc, StoreError> {
        let revision = body
            .pointer("/metadata/resourceVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| StoreError::Failed("the API returned no resourceVersion".to_string()))?;
        let mut data = Data::new();
        if let Some(fields) = body.get("data").and_then(Value::as_object) {
            for (key, value) in fields {
                let text = value.as_str().unwrap_or_default();
                let text = match kind {
                    Kind::ConfigMap => text.to_string(),
                    Kind::Secret => {
                        let bytes = STANDARD.decode(text).map_err(|err| {
                            StoreError::Failed(format!("{key} isn't valid base64: {err}"))
                        })?;
                        String::from_utf8(bytes)
                            .map_err(|_| StoreError::Failed(format!("{key} isn't text")))?
                    }
                };
                data.insert(key.clone(), text);
            }
        }
        Ok(Doc::new(data, revision.to_string()))
    }

    async fn get(&self, kind: Kind) -> Result<Doc, StoreError> {
        let request = self.authorized(self.http.get(self.url(kind)))?;
        let response = request
            .send()
            .await
            .map_err(|err| StoreError::Failed(crate::upstream::describe(&err)))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|err| StoreError::Failed(crate::upstream::describe(&err)))?;
        if !status.is_success() {
            return Err(self.refused(kind, status, &text));
        }
        let body: Value = serde_json::from_str(&text)
            .map_err(|err| StoreError::Failed(format!("unexpected API answer: {err}")))?;
        Self::decode(kind, &body)
    }

    fn patch_body(kind: Kind, doc: &Doc) -> Value {
        let mut fields = Map::new();
        for (key, value) in &doc.data {
            let value = match kind {
                Kind::ConfigMap => value.clone(),
                Kind::Secret => STANDARD.encode(value),
            };
            fields.insert(key.clone(), Value::String(value));
        }
        for gone in doc
            .read_keys
            .iter()
            .filter(|key| !doc.data.contains_key(*key))
        {
            fields.insert(gone.clone(), Value::Null);
        }
        json!({ "metadata": { "resourceVersion": doc.revision }, "data": fields })
    }

    async fn put(&self, kind: Kind, doc: &Doc) -> Result<Doc, StoreError> {
        let request = self
            .http
            .patch(self.url(kind))
            .header("content-type", "application/merge-patch+json")
            .body(Self::patch_body(kind, doc).to_string());
        let response = self
            .authorized(request)?
            .send()
            .await
            .map_err(|err| StoreError::Failed(crate::upstream::describe(&err)))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|err| StoreError::Failed(crate::upstream::describe(&err)))?;
        if status == StatusCode::CONFLICT {
            return Err(StoreError::Conflict);
        }
        if !status.is_success() {
            return Err(self.refused(kind, status, &text));
        }
        let body: Value = serde_json::from_str(&text)
            .map_err(|err| StoreError::Failed(format!("unexpected API answer: {err}")))?;
        Self::decode(kind, &body)
    }
}

pub enum ConfigStore {
    Local(LocalStore),
    Kube(KubeStore),
}

impl ConfigStore {
    pub async fn restart_outbox(&self, kind: &str, deployment: &str) -> Result<(), StoreError> {
        let Self::Kube(store) = self else {
            return Err(StoreError::Failed(
                "The local store does not supervise processes. Run `mmx local restart outbox` for Docker Compose, or restart mmx-outbox with your service manager.".to_string(),
            ));
        };
        if deployment.is_empty()
            || !deployment
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.')
        {
            return Err(StoreError::Failed(
                "Invalid outbox Deployment name".to_string(),
            ));
        }
        let resource = match kind {
            "deployment" => "deployments",
            "statefulset" => "statefulsets",
            _ => {
                return Err(StoreError::Failed(
                    "Invalid outbox workload kind".to_string(),
                ));
            }
        };
        let url = format!(
            "{}/apis/apps/v1/namespaces/{}/{resource}/{deployment}",
            store.api, store.namespace
        );
        let patch = json!({ "spec": { "template": { "metadata": { "annotations": {
            "mmx.exchange/restart-id": uuid::Uuid::new_v4().to_string()
        }}}}});
        let response = store
            .authorized(
                store
                    .http
                    .patch(url)
                    .header("content-type", "application/merge-patch+json")
                    .json(&patch),
            )?
            .send()
            .await
            .map_err(|err| StoreError::Failed(err.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(StoreError::Failed(format!(
                "Restarting outbox Deployment {deployment}: HTTP {status}"
            )));
        }
        Ok(())
    }

    pub fn info(&self) -> StoreInfo {
        match self {
            Self::Local(store) => StoreInfo {
                backend: "local",
                config: store.markets_file.display().to_string(),
                secrets: store.dir.join(SECRETS_FILE).display().to_string(),
                delivery: Delivery::Immediate,
            },
            Self::Kube(store) => StoreInfo {
                backend: "kubernetes",
                config: store.describe(Kind::ConfigMap),
                secrets: store.describe(Kind::Secret),
                delivery: Delivery::Eventual,
            },
        }
    }

    pub async fn config(&self) -> Result<Doc, StoreError> {
        match self {
            Self::Local(store) => store.read_config(),
            Self::Kube(store) => store.get(Kind::ConfigMap).await,
        }
    }

    pub async fn secrets(&self) -> Result<Doc, StoreError> {
        match self {
            Self::Local(store) => store.read_secrets(),
            Self::Kube(store) => store.get(Kind::Secret).await,
        }
    }

    pub async fn put_config(&self, doc: &Doc) -> Result<Doc, StoreError> {
        match self {
            Self::Local(store) => store.put_config(doc),
            Self::Kube(store) => store.put(Kind::ConfigMap, doc).await,
        }
    }

    pub async fn put_secrets(&self, doc: &Doc) -> Result<Doc, StoreError> {
        match self {
            Self::Local(store) => store.put_secrets(doc),
            Self::Kube(store) => store.put(Kind::Secret, doc).await,
        }
    }

    /// Reads, changes and writes, starting over if something else got in first. `change` may run
    /// more than once, and returning an error abandons the write.
    pub async fn update_config<F>(&self, mut change: F) -> Result<Doc, StoreError>
    where
        F: FnMut(&mut Data) -> Result<(), StoreError>,
    {
        for _ in 0..UPDATE_ATTEMPTS {
            let mut doc = self.config().await?;
            change(&mut doc.data)?;
            match self.put_config(&doc).await {
                Err(StoreError::Conflict) => {}
                result => return result,
            }
        }
        Err(StoreError::Failed(MISSING_KEY.to_string()))
    }

    pub async fn update_secrets<F>(&self, mut change: F) -> Result<Doc, StoreError>
    where
        F: FnMut(&mut Data) -> Result<(), StoreError>,
    {
        for _ in 0..UPDATE_ATTEMPTS {
            let mut doc = self.secrets().await?;
            change(&mut doc.data)?;
            match self.put_secrets(&doc).await {
                Err(StoreError::Conflict) => {}
                result => return result,
            }
        }
        Err(StoreError::Failed(MISSING_KEY.to_string()))
    }
}

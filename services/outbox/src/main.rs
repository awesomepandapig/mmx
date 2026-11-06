mod tape;

use outbox::delivery::{Deliverer, DeliveryConfig};
use outbox::outbox::{Error, Outbox, SyncConfig};
use outbox::status::{self, WebhookStatus};
use outbox::webhook;

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::exit;
use std::thread;
use std::time::Duration;

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}

fn optional<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|value| value.parse().ok()).unwrap_or(default)
}

fn setting(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn serve_status(dir: PathBuf, outbox: &Outbox, webhook: WebhookStatus) {
    let listener = TcpListener::bind(std::env::var("OUTBOX_HTTP").unwrap_or_else(|_| "0.0.0.0:8092".to_string()))
        .expect("failed to bind the status address");
    let shared = outbox.shared();
    thread::spawn(move || status::serve(&listener, dir, shared, webhook));
}

fn main() {
    let dir = PathBuf::from(required("OUTBOX_DIR"));

    let sync = SyncConfig {
        max_pending_bytes: optional("OUTBOX_SYNC_BYTES", SyncConfig::default().max_pending_bytes),
        max_delay: Duration::from_micros(optional("OUTBOX_SYNC_DELAY_US", 1000)),
    };

    let mut outbox = match Outbox::open(&dir, sync) {
        Ok(outbox) => outbox,
        Err(error) => {
            eprintln!("can't open the outbox: {error}");
            exit(1);
        }
    };
    let shared = outbox.shared();
    eprintln!("outbox open at {}, tracking {} open orders", outbox.position(), outbox.open_orders());

    // A fresh install has no webhook yet, and crash-looping until it does says nothing useful. The
    // tape keeps everything, so a restart with settings starts from where this journal stands.
    let (Some(url), Some(secret)) = (setting("WEBHOOK_URL"), setting("WEBHOOK_SECRET")) else {
        eprintln!("no webhook is configured, so waiting for a restart with one");
        serve_status(dir, &outbox, WebhookStatus {
            configured: false,
            webhook_url: setting("WEBHOOK_URL"),
            config_id: None,
        });
        while !shared.is_stopping() {
            thread::sleep(Duration::from_secs(1));
        }
        return;
    };

    let webhook = WebhookStatus {
        configured: true,
        config_id: Some(webhook::config_id(&url, &secret)),
        webhook_url: Some(url.clone()),
    };
    let mut delivery = DeliveryConfig::new(url, secret.into_bytes());
    delivery.max_events = optional("WEBHOOK_MAX_EVENTS", delivery.max_events);

    let deliverer = match Deliverer::open(&dir, outbox.shared(), delivery) {
        Ok(deliverer) => deliverer,
        Err(error) => {
            eprintln!("can't start delivery: {error}");
            exit(1);
        }
    };
    thread::spawn(move || {
        if let Err(error) = deliverer.run() {
            eprintln!("delivery failed: {error}");
            exit(1);
        }
    });

    serve_status(dir, &outbox, webhook);

    match tape::follow(&mut outbox, optional("CLUSTER_NODES", 3)) {
        // Stays up, so that `/position` can say why, and the engine can still read the position.
        Error::Halted(reason) => {
            eprintln!("waiting for an operator: {reason}");
            while !shared.is_stopping() {
                thread::sleep(Duration::from_secs(1));
            }
        }
        Error::Io(error) => {
            eprintln!("disk failure, exiting: {error}");
            exit(1);
        }
    }
}

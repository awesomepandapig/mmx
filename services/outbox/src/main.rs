mod tape;

use outbox::delivery::{Deliverer, DeliveryConfig};
use outbox::outbox::{Error, Outbox, SyncConfig};
use outbox::status;

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

fn main() {
    let dir = PathBuf::from(required("OUTBOX_DIR"));
    let url = required("WEBHOOK_URL");
    let secret = required("WEBHOOK_SECRET");
    assert!(!secret.is_empty(), "WEBHOOK_SECRET can't be empty");

    let sync = SyncConfig {
        max_pending_bytes: optional("OUTBOX_SYNC_BYTES", SyncConfig::default().max_pending_bytes),
        max_delay: Duration::from_micros(optional("OUTBOX_SYNC_DELAY_US", 1000)),
    };
    let mut delivery = DeliveryConfig::new(url, secret.into_bytes());
    delivery.max_events = optional("WEBHOOK_MAX_EVENTS", delivery.max_events);

    let mut outbox = match Outbox::open(&dir, sync) {
        Ok(outbox) => outbox,
        Err(error) => {
            eprintln!("can't open the outbox: {error}");
            exit(1);
        }
    };
    let shared = outbox.shared();
    eprintln!("outbox open at {}, tracking {} open orders", outbox.position(), outbox.open_orders());

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

    let listener = TcpListener::bind(std::env::var("OUTBOX_HTTP").unwrap_or_else(|_| "0.0.0.0:8092".to_string()))
        .expect("failed to bind the status address");
    let status_dir = dir.clone();
    let status_shared = outbox.shared();
    thread::spawn(move || status::serve(&listener, status_dir, status_shared));

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

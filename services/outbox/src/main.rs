mod tape;

use outbox::delivery::{self, Deliverer, DeliveryConfig};
use outbox::outbox::{Error, Outbox, SyncConfig};
use outbox::status::{self, WebhookStatus};
use outbox::webhook;

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::exit;
use std::thread;
use std::time::Duration;

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        eprintln!("{name} is required and must contain valid Unicode");
        exit(1)
    })
}

fn parse_optional<T: std::str::FromStr>(
    name: &str,
    value: Option<&str>,
    default: T,
) -> Result<T, String> {
    match value {
        None => Ok(default),
        Some(value) => value
            .parse()
            .map_err(|_| format!("{name} has an invalid value")),
    }
}

fn optional<T: std::str::FromStr>(name: &str, default: T) -> T {
    let value = match std::env::var(name) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => {
            eprintln!("{name} must contain valid Unicode");
            exit(1);
        }
    };
    parse_optional(name, value.as_deref(), default).unwrap_or_else(|error| {
        eprintln!("{error}");
        exit(1)
    })
}

fn setting(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn serve_status(dir: PathBuf, outbox: &Outbox, webhook: WebhookStatus) {
    let listener = TcpListener::bind(optional("OUTBOX_HTTP", "0.0.0.0:8092".to_string()))
        .unwrap_or_else(|error| {
            eprintln!("cannot bind the status address: {error}");
            exit(1)
        });
    let shared = outbox.shared();
    if let Err(error) = thread::Builder::new()
        .name("outbox-status".into())
        .spawn(move || status::serve(&listener, dir, shared, webhook))
    {
        eprintln!("cannot start the status server: {error}");
        exit(1);
    }
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
    eprintln!(
        "outbox open at {}, tracking {} open orders",
        outbox.position(),
        outbox.open_orders()
    );

    // A fresh install has no webhook yet, and crash-looping until it does says nothing useful. The
    // tape keeps everything, so a restart with settings starts from where this journal stands.
    let (Some(url), Some(secret)) = (setting("WEBHOOK_URL"), setting("WEBHOOK_SECRET")) else {
        eprintln!("no webhook is configured, so waiting for a restart with one");
        serve_status(
            dir,
            &outbox,
            WebhookStatus {
                configured: false,
                webhook_url: setting("WEBHOOK_URL"),
                config_id: None,
            },
        );
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
    let sender_shared = outbox.shared();
    if let Err(error) = thread::Builder::new()
        .name("outbox-delivery".into())
        .spawn(move || {
            if let Err(error) = delivery::supervise(|| deliverer.run()) {
                eprintln!("delivery failed: {error}");
                exit(1);
            }
            if !sender_shared.is_stopping() {
                eprintln!(
                    "delivery stopped unexpectedly; refusing to remain healthy without a sender"
                );
                exit(1);
            }
        })
    {
        eprintln!("cannot start delivery: {error}");
        exit(1);
    }

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

#[cfg(test)]
mod tests {
    use super::parse_optional;

    #[test]
    fn invalid_explicit_configuration_does_not_use_the_default() {
        assert_eq!(
            parse_optional("WEBHOOK_MAX_EVENTS", None, 100usize),
            Ok(100)
        );
        assert_eq!(
            parse_optional("WEBHOOK_MAX_EVENTS", Some("7"), 100usize),
            Ok(7)
        );
        for value in ["", " ", "invalid", "-1", "184467440737095516160"] {
            assert!(parse_optional("WEBHOOK_MAX_EVENTS", Some(value), 100usize).is_err());
        }
    }
}

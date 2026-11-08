//! A small HTTP endpoint for the engine's startup check and for operators.
//!
//! - `GET /position`: the durable position and the webhook settings in use, as JSON
//! - `GET /journal?from=P&limit=N`: journal records after position `P`, in the journal's own format
//! - `GET /healthz`: 200 while the process is up
//! - `GET /readyz`: 200 while delivery is allowed, otherwise 503, including with no webhook set
//!
//! It only starts once the journal has been recovered, so every answer reflects the disk.

use crate::journal::JournalReader;
use crate::outbox::{Position, Shared};

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use serde::Serialize;

const MAX_REQUEST: usize = 4096;
const TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_LIMIT: usize = 10_000;
const MAX_JOURNAL_BYTES: usize = 1024 * 1024;
const MAX_CONNECTIONS: usize = 16;
const MAX_EXPORTS: usize = 2;

struct Permit(Arc<AtomicUsize>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn acquire(active: &Arc<AtomicUsize>, limit: usize) -> Option<Permit> {
    let mut count = active.load(Ordering::Acquire);
    loop {
        if count >= limit {
            return None;
        }
        match active.compare_exchange_weak(count, count + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(Permit(Arc::clone(active))),
            Err(actual) => count = actual,
        }
    }
}

/// What this process started with. The URL isn't secret, and `config_id` lets `mmx serve` tell
/// whether saved settings have been picked up, without either side showing the signing secret.
#[derive(Serialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct WebhookStatus {
    pub configured: bool,
    pub webhook_url: Option<String>,
    pub config_id: Option<String>,
}

#[derive(Serialize)]
struct PositionReply {
    #[serde(flatten)]
    position: Position,
    #[serde(flatten)]
    webhook: WebhookStatus,
}

struct Response {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn text(status: &'static str, body: &str) -> Self {
        Self {
            status,
            content_type: "text/plain",
            body: body.as_bytes().to_vec(),
        }
    }
}

fn query_value(query: &str, name: &str) -> Result<Option<u64>, String> {
    let mut result = None;
    for pair in query.split('&') {
        let Some(value) = pair
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('='))
        else {
            continue;
        };
        if result.is_some() || value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(format!("invalid or repeated {name}"));
        }
        result = Some(value.parse().map_err(|_| format!("{name} is too large"))?);
    }
    Ok(result)
}

/// Returns up to `limit` durable records after `from`, for rebuilding what the tape lost.
fn journal_after(dir: &Path, shared: &Shared, from: i64, limit: usize) -> io::Result<Vec<u8>> {
    let durable = shared.durable_length();
    let mut reader = JournalReader::open(dir, 0)?;
    let mut body = Vec::new();
    let mut records = 0;
    while records < limit {
        let Some(record) = reader.next_below(durable)? else {
            break;
        };
        if record.position > from {
            let mut encoded = Vec::new();
            record.encode_into(&mut encoded);
            if body.len() + encoded.len() > MAX_JOURNAL_BYTES {
                break;
            }
            body.extend_from_slice(&encoded);
            records += 1;
        }
    }
    Ok(body)
}

fn route(
    target: &str,
    dir: &Path,
    shared: &Shared,
    webhook: &WebhookStatus,
    exports: &Arc<AtomicUsize>,
) -> Response {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match path {
        "/position" => match serde_json::to_vec(&PositionReply {
            position: shared.position(),
            webhook: webhook.clone(),
        }) {
            Ok(body) => Response {
                status: "200 OK",
                content_type: "application/json",
                body,
            },
            Err(error) => Response::text("500 Internal Server Error", &error.to_string()),
        },
        "/journal" => {
            let parsed = (|| -> Result<(i64, usize), String> {
                let from = i64::try_from(query_value(query, "from")?.unwrap_or(0))
                    .map_err(|_| "from is outside the supported position range".to_string())?;
                let limit =
                    usize::try_from(query_value(query, "limit")?.unwrap_or(DEFAULT_LIMIT as u64))
                        .map_err(|_| "limit is outside the supported range".to_string())?;
                if !(1..=DEFAULT_LIMIT).contains(&limit) {
                    return Err(format!("limit must be between 1 and {DEFAULT_LIMIT}"));
                }
                Ok((from, limit))
            })();
            let (from, limit) = match parsed {
                Ok(parsed) => parsed,
                Err(error) => return Response::text("400 Bad Request", &error),
            };
            let Some(_permit) = acquire(exports, MAX_EXPORTS) else {
                return Response::text(
                    "503 Service Unavailable",
                    "journal export capacity is busy; retry later",
                );
            };
            match journal_after(dir, shared, from, limit) {
                Ok(body) => Response {
                    status: "200 OK",
                    content_type: "application/octet-stream",
                    body,
                },
                Err(error) => Response::text("500 Internal Server Error", &error.to_string()),
            }
        }
        "/healthz" => Response::text("200 OK", "ok"),
        "/readyz" if webhook.configured && shared.may_deliver() => {
            Response::text("200 OK", "ready")
        }
        "/readyz" => Response::text("503 Service Unavailable", "not ready"),
        _ => Response::text("404 Not Found", "not found"),
    }
}

fn read_request_target(stream: &mut TcpStream) -> io::Result<Option<String>> {
    let mut request = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") && request.len() < MAX_REQUEST {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
    }
    let text = String::from_utf8_lossy(&request);
    let mut line = text.lines().next().unwrap_or("").split_whitespace();
    Ok(match (line.next(), line.next()) {
        (Some("GET"), Some(target)) => Some(target.to_string()),
        _ => None,
    })
}

fn handle(
    mut stream: TcpStream,
    dir: &Path,
    shared: &Shared,
    webhook: &WebhookStatus,
    exports: &Arc<AtomicUsize>,
) -> io::Result<()> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let response = match read_request_target(&mut stream)? {
        Some(target) => route(&target, dir, shared, webhook, exports),
        None => Response::text("405 Method Not Allowed", "only GET is supported"),
    };
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)
}

/// Bounded concurrency keeps journal scans/slow clients from serializing witness and probe reads.
/// At most two workers materialize exports; the remaining slots can answer status requests.
pub fn serve(listener: &TcpListener, dir: PathBuf, shared: Arc<Shared>, webhook: WebhookStatus) {
    let active = Arc::new(AtomicUsize::new(0));
    let exports = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("status accept failed: {error}");
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        let Some(permit) = acquire(&active, MAX_CONNECTIONS) else {
            // Close immediately rather than block the accept loop on an overloaded/slow client.
            drop(stream);
            continue;
        };
        let (dir, shared, webhook, exports) = (
            dir.clone(),
            Arc::clone(&shared),
            webhook.clone(),
            Arc::clone(&exports),
        );
        if let Err(error) = thread::Builder::new()
            .name("outbox-status-request".into())
            .spawn(move || {
                let _permit = permit;
                if let Err(error) = handle(stream, &dir, &shared, &webhook, &exports) {
                    eprintln!("status request failed: {error}");
                }
            })
        {
            eprintln!("cannot start status request worker: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{self, FRAME_STRIDE, Report, TestDir};
    use crate::outbox::{Outbox, SyncConfig};
    use sbe::{exec_type_enum::ExecTypeEnum, ord_status_enum::OrdStatusEnum};

    fn outbox(dir: &Path) -> Outbox {
        Outbox::open(
            dir,
            SyncConfig {
                max_pending_bytes: usize::MAX,
                max_delay: Duration::from_secs(3600),
            },
        )
        .expect("outbox")
    }

    #[test]
    fn malformed_export_arguments_are_not_silent_defaults_or_wrapping_positions() {
        let dir = TestDir::new("export-arguments");
        let outbox = outbox(&dir.0);
        let exports = Arc::new(AtomicUsize::new(0));
        for query in [
            "limit=0",
            "limit=10001",
            "limit=no",
            "limit=-1",
            "limit=1&limit=2",
            "from=18446744073709551615",
            "from=-1",
            "from=",
            "from=1&from=2",
        ] {
            assert_eq!(
                route(
                    &format!("/journal?{query}"),
                    &dir.0,
                    &outbox.shared(),
                    &WebhookStatus::default(),
                    &exports
                )
                .status,
                "400 Bad Request",
                "{query}"
            );
        }
    }

    #[test]
    fn journal_pages_are_byte_bounded_and_continue_without_skipped_records() {
        let dir = TestDir::new("export-pages");
        let mut outbox = outbox(&dir.0);
        let mut expected = Vec::new();
        let mut record_size = 0;
        for index in 1..=DEFAULT_LIMIT {
            let position = index as i64 * FRAME_STRIDE;
            let frame = fixture::report(Report {
                exec_id: index as u64,
                order_id: index as u64,
                exec_type: ExecTypeEnum::Rejected,
                ord_status: OrdStatusEnum::Rejected,
                ..Report::default()
            });
            outbox.ingest(position, &frame).expect("ingest");
            let start = expected.len();
            crate::journal::Record {
                position,
                exec_id: index as u64,
                flags: 0,
                frame,
            }
            .encode_into(&mut expected);
            record_size = expected.len() - start;
        }
        outbox.sync().expect("sync");
        assert!(
            expected.len() > MAX_JOURNAL_BYTES,
            "fixture must cross byte cap"
        );
        let first = journal_after(&dir.0, &outbox.shared(), 0, DEFAULT_LIMIT).expect("page");
        assert!(first.len() <= MAX_JOURNAL_BYTES);
        assert_eq!(first.len() % record_size, 0, "never split a record");
        assert_eq!(first.len(), MAX_JOURNAL_BYTES / record_size * record_size);
        let last_position = (first.len() / record_size) as i64 * FRAME_STRIDE;
        let next = journal_after(&dir.0, &outbox.shared(), last_position, DEFAULT_LIMIT)
            .expect("next page");
        let mut combined = first;
        combined.extend(next);
        assert_eq!(combined, expected);
    }

    #[test]
    fn busy_exports_do_not_block_position_or_health_and_capacity_is_released() {
        let dir = TestDir::new("export-slots");
        let outbox = outbox(&dir.0);
        let exports = Arc::new(AtomicUsize::new(0));
        let first = acquire(&exports, MAX_EXPORTS).expect("first export");
        let second = acquire(&exports, MAX_EXPORTS).expect("second export");
        let webhook = WebhookStatus::default();
        assert_eq!(
            route("/journal", &dir.0, &outbox.shared(), &webhook, &exports).status,
            "503 Service Unavailable"
        );
        assert_eq!(
            route("/position", &dir.0, &outbox.shared(), &webhook, &exports).status,
            "200 OK"
        );
        assert_eq!(
            route("/healthz", &dir.0, &outbox.shared(), &webhook, &exports).status,
            "200 OK"
        );
        drop(first);
        assert_eq!(
            route("/journal", &dir.0, &outbox.shared(), &webhook, &exports).status,
            "200 OK"
        );
        drop(second);
        assert_eq!(exports.load(Ordering::Acquire), 0);
    }

    #[test]
    fn slow_request_headers_do_not_serialize_health_queries() {
        let dir = TestDir::new("status-concurrency");
        let outbox = outbox(&dir.0);
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let (directory, shared) = (dir.0.clone(), outbox.shared());
        thread::spawn(move || serve(&listener, directory, shared, WebhookStatus::default()));
        let mut slow = TcpStream::connect(address).expect("slow client");
        slow.write_all(b"GET /journal HTTP/1.1\r\n")
            .expect("partial request");
        thread::sleep(Duration::from_millis(30));
        let mut fast = TcpStream::connect(address).expect("probe");
        fast.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("timeout");
        fast.write_all(b"GET /healthz HTTP/1.1\r\nHost: outbox\r\n\r\n")
            .expect("probe request");
        let mut response = String::new();
        fast.read_to_string(&mut response)
            .expect("probe must not wait for slow headers");
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        drop(slow);
    }
}

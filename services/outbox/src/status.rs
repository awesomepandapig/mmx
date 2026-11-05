//! A small HTTP endpoint for the engine's startup check and for operators.
//!
//! - `GET /position`: the durable position, as JSON
//! - `GET /journal?from=P&limit=N`: journal records after position `P`, in the journal's own format
//! - `GET /healthz`: 200 while the process is up
//! - `GET /readyz`: 200 while delivery is allowed, otherwise 503
//!
//! It only starts once the journal has been recovered, so every answer reflects the disk.

use crate::journal::JournalReader;
use crate::outbox::Shared;

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const MAX_REQUEST: usize = 4096;
const TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_LIMIT: usize = 100_000;

struct Response {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn text(status: &'static str, body: &str) -> Self {
        Self { status, content_type: "text/plain", body: body.as_bytes().to_vec() }
    }
}

fn query_value(query: &str, name: &str) -> Option<u64> {
    query.split('&').find_map(|pair| pair.strip_prefix(name)?.strip_prefix('=')?.parse().ok())
}

/// Returns up to `limit` durable records after `from`, for rebuilding what the tape lost.
fn journal_after(dir: &Path, shared: &Shared, from: i64, limit: usize) -> io::Result<Vec<u8>> {
    let durable = shared.durable_length();
    let mut reader = JournalReader::open(dir, 0)?;
    let mut body = Vec::new();
    let mut records = 0;
    while records < limit {
        let Some(record) = reader.next_below(durable)? else { break };
        if record.position > from {
            record.encode_into(&mut body);
            records += 1;
        }
    }
    Ok(body)
}

fn route(target: &str, dir: &Path, shared: &Shared) -> Response {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match path {
        "/position" => Response {
            status: "200 OK",
            content_type: "application/json",
            body: serde_json::to_vec(&shared.position()).expect("position serializes"),
        },
        "/journal" => {
            let from = query_value(query, "from").unwrap_or(0);
            let limit = query_value(query, "limit").map_or(DEFAULT_LIMIT, |limit| limit as usize);
            match journal_after(dir, shared, from as i64, limit) {
                Ok(body) => Response { status: "200 OK", content_type: "application/octet-stream", body },
                Err(error) => Response::text("500 Internal Server Error", &error.to_string()),
            }
        }
        "/healthz" => Response::text("200 OK", "ok"),
        "/readyz" if shared.may_deliver() => Response::text("200 OK", "ready"),
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

fn handle(mut stream: TcpStream, dir: &Path, shared: &Shared) -> io::Result<()> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let response = match read_request_target(&mut stream)? {
        Some(target) => route(&target, dir, shared),
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

/// Serves until the process exits, one request at a time.
pub fn serve(listener: &TcpListener, dir: PathBuf, shared: Arc<Shared>) {
    for stream in listener.incoming() {
        let result = stream.and_then(|stream| handle(stream, &dir, &shared));
        if let Err(error) = result {
            eprintln!("status request failed: {error}");
        }
    }
}

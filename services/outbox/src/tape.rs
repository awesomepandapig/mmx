//! Follows the recorded report stream from any node.
//!
//! Every node records the same bytes, so a saved position means the same thing on all of them.
//! A node that's restarting can be briefly behind, because it truncates its recording back to a
//! snapshot and rebuilds it. So a node that's behind the journal is skipped, and never treated as a
//! total loss here: the engine makes that call, using `GET /position`.

use outbox::outbox::{Error, Outbox};

use rusteron_archive as archive;

use std::ffi::CString;
use std::thread::sleep;
use std::time::{Duration, Instant};

const REPORT_STREAM: i32 = 1002;
const REPORT_SESSION: i32 = 1002;
/// Each node gets its own replay stream on this client's driver.
const REPLAY_STREAM_BASE: i32 = 3040;
const FRAGMENT_LIMIT: usize = 256;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(400);
const RETRY_AFTER: Duration = Duration::from_secs(1);
/// A source that's been quiet this long gets compared with the other nodes. It may have died.
const CHECK_QUIET_AFTER: Duration = Duration::from_secs(5);
const IDLE: Duration = Duration::from_micros(100);

/// Match the endpoint contract used by the other tape consumers. Explicit paired endpoints
/// take precedence over the legacy host list, so local source instances can use distinct ports.
struct Endpoints {
    archives: Vec<String>,
    reports: Vec<String>,
}

impl Endpoints {
    fn parse(
        nodes: usize,
        archives: Option<&str>,
        reports: Option<&str>,
        hosts: Option<&str>,
    ) -> Result<Self, String> {
        if nodes == 0 {
            return Err("CLUSTER_NODES must be positive".into());
        }
        let list = |text: &str| {
            text.split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        match (archives, reports) {
            (Some(archives), Some(reports)) => {
                let archives = list(archives);
                let reports = list(reports);
                if archives.len() != nodes || reports.len() != nodes {
                    return Err(
                        "TAPE_ARCHIVES and TAPE_REPORTS need one endpoint per cluster node".into(),
                    );
                }
                for endpoint in archives.iter().chain(&reports) {
                    let Some((host, port)) = endpoint.rsplit_once(':') else {
                        return Err(format!("invalid tape endpoint: {endpoint}"));
                    };
                    if host.is_empty()
                        || host.chars().any(char::is_whitespace)
                        || port.parse::<u16>().is_err()
                        || port == "0"
                    {
                        return Err(format!("invalid tape endpoint: {endpoint}"));
                    }
                }
                Ok(Self { archives, reports })
            }
            (None, None) => {
                let hosts = hosts
                    .map(list)
                    .unwrap_or_else(|| vec!["localhost".into(); nodes]);
                if hosts.len() != nodes {
                    return Err("CLUSTER_HOSTS needs one host per cluster node".into());
                }
                Ok(Self {
                    archives: hosts
                        .iter()
                        .enumerate()
                        .map(|(i, host)| format!("{host}:{}", 8010 + i))
                        .collect(),
                    reports: hosts
                        .iter()
                        .enumerate()
                        .map(|(i, host)| format!("{host}:{}", 40450 + i))
                        .collect(),
                })
            }
            _ => Err("TAPE_ARCHIVES and TAPE_REPORTS must be configured together".into()),
        }
    }
}

fn client_host() -> String {
    std::env::var("CLIENT_HOST").unwrap_or_else(|_| "localhost".to_string())
}

fn cstring(value: &str) -> CString {
    CString::new(value).expect("no nul")
}

fn archive_client() -> archive::Aeron {
    let context = archive::AeronContext::new().expect("failed to create a client context");
    context
        .set_dir(&cstring(&transport::aeron_dir()))
        .expect("invalid Aeron dir");
    context
        .set_error_handler(Some(|code: i32, message: &str| {
            eprintln!("archive client error {code}: {message}")
        }))
        .expect("failed to set the error handler");
    let aeron = archive::Aeron::new(&context).expect("failed to create a client");
    aeron.start().expect("failed to start the client");
    aeron
}

struct Source {
    node: usize,
    context: archive::AeronArchiveContext,
    /// Stays open until the subscription has been built from the same context.
    _client: archive::AeronArchive,
    recording_id: i64,
    recorded: i64,
}

/// Finds a node's report recording, or `None` if the node is unreachable.
fn probe(aeron: &archive::Aeron, node: usize, endpoints: &Endpoints) -> Option<Source> {
    let context = archive::AeronArchiveContext::new().ok()?;
    context.set_aeron(aeron).ok()?;
    context
        .set_control_request_channel(&cstring(&format!(
            "aeron:udp?endpoint={}",
            endpoints.archives[node]
        )))
        .ok()?;
    context
        .set_control_response_channel(&cstring(&format!("aeron:udp?endpoint={}:0", client_host())))
        .ok()?;

    let client = archive::AeronArchiveAsyncConnect::new_with_aeron(&context, aeron)
        .ok()?
        .poll_blocking(CONNECT_TIMEOUT)
        .ok()?;
    let recording_id = client
        .find_last_matching_recording(0, &cstring(""), REPORT_STREAM, REPORT_SESSION)
        .ok()?;
    if recording_id < 0 {
        return None;
    }
    let recorded = client.get_max_recorded_position(recording_id).ok()?;
    Some(Source {
        node,
        context,
        _client: client,
        recording_id,
        recorded,
    })
}

/// The node with the most tape, which is the safest one to read.
fn furthest(aeron: &archive::Aeron, nodes: usize, endpoints: &Endpoints) -> Option<Source> {
    (0..nodes)
        .filter_map(|node| probe(aeron, node, endpoints))
        .max_by_key(|source| source.recorded)
}

fn subscribe(
    aeron: &archive::Aeron,
    source: &Source,
    position: i64,
    endpoints: &Endpoints,
) -> Option<archive::AeronArchivePersistentSubscription> {
    let node = source.node;
    archive::PersistentSubscriptionBuilder::new_with_aeron(&source.context, aeron)
        .ok()?
        .live_channel(&format!(
            "aeron:udp?endpoint={}:0|control={}|session-id={REPORT_SESSION}",
            client_host(),
            endpoints.reports[node]
        ))
        .ok()?
        .live_stream_id(REPORT_STREAM)
        .ok()?
        .replay_channel(&format!("aeron:udp?endpoint={}:0", client_host()))
        .ok()?
        .replay_stream_id(REPLAY_STREAM_BASE + node as i32)
        .ok()?
        .start_position(position)
        .ok()?
        .recording_id(source.recording_id)
        .ok()?
        .build()
        .ok()
}

/// Feeds the outbox from the tape until it fails or halts.
pub fn follow(outbox: &mut Outbox, nodes: usize) -> Error {
    let archives = std::env::var("TAPE_ARCHIVES").ok();
    let reports = std::env::var("TAPE_REPORTS").ok();
    let hosts = std::env::var("CLUSTER_HOSTS").ok();
    let endpoints = Endpoints::parse(
        nodes,
        archives.as_deref(),
        reports.as_deref(),
        hosts.as_deref(),
    )
    .unwrap_or_else(|reason| {
        eprintln!("invalid outbox tape configuration: {reason}");
        std::process::exit(1);
    });
    let mut aeron = archive_client();
    let mut subscription: Option<archive::AeronArchivePersistentSubscription> = None;
    let mut current_node = None;
    let mut last_frame = Instant::now();
    let mut last_attempt: Option<Instant> = None;

    loop {
        let mut frames = 0;
        let mut failure = None;
        if let Some(subscription) = &subscription {
            let _ = subscription.poll_fn(
                |buffer, header| {
                    frames += 1;
                    if failure.is_none()
                        && let Err(error) = outbox.ingest(header.position(), buffer)
                    {
                        failure = Some(error);
                    }
                },
                FRAGMENT_LIMIT,
            );
        }
        if let Some(error) = failure {
            return error;
        }
        if let Err(error) = outbox.tick() {
            return error;
        }

        if frames > 0 {
            last_frame = Instant::now();
            continue;
        }

        let failed = subscription
            .as_ref()
            .is_none_or(archive::AeronArchivePersistentSubscription::has_failed);
        let quiet = last_frame.elapsed() >= CHECK_QUIET_AFTER;
        if (failed || quiet) && last_attempt.is_none_or(|attempt| attempt.elapsed() >= RETRY_AFTER)
        {
            last_attempt = Some(Instant::now());
            if quiet {
                last_frame = Instant::now();
            }
            if let Err(error) = outbox.sync() {
                return error;
            }
            let position = outbox.position();
            match furthest(&aeron, nodes, &endpoints) {
                Some(source) if source.recorded < position => {
                    eprintln!(
                        "no node has the journal's tape: best is {} at {}, journal is at {position}",
                        source.node, source.recorded
                    );
                    if failed {
                        subscription = None;
                        current_node = None;
                    }
                }
                Some(source)
                    if failed
                        || (source.recorded > position && current_node != Some(source.node)) =>
                {
                    let resume = outbox.resume_position();
                    let node = source.node;
                    eprintln!("reading the tape from node {node} at {resume}");
                    // Closing a subscription leaves its images mapped until the client closes,
                    // and each one holds 192 MB of shared memory.
                    drop((subscription.take(), source));
                    aeron = archive_client();
                    subscription = probe(&aeron, node, &endpoints)
                        .and_then(|source| subscribe(&aeron, &source, resume, &endpoints));
                    current_node = subscription.is_some().then_some(node);
                }
                Some(_) => {}
                None if failed => {
                    eprintln!("no node's archive is reachable");
                    subscription = None;
                    current_node = None;
                }
                None => {}
            }
        }
        sleep(IDLE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paired_tape_endpoints_override_legacy_hosts_and_preserve_custom_ports() {
        let endpoints = Endpoints::parse(
            2,
            Some("one:18010, two:18011,"),
            Some("one:50450,two:50451"),
            Some("ignored,ignored"),
        )
        .expect("paired endpoints");
        assert_eq!(endpoints.archives, ["one:18010", "two:18011"]);
        assert_eq!(endpoints.reports, ["one:50450", "two:50451"]);
    }

    #[test]
    fn legacy_hosts_and_default_local_ports_remain_supported() {
        let local = Endpoints::parse(3, None, None, None).expect("default");
        assert_eq!(
            local.archives,
            ["localhost:8010", "localhost:8011", "localhost:8012"]
        );
        let hosts = Endpoints::parse(2, None, None, Some("a,b")).expect("legacy hosts");
        assert_eq!(hosts.reports, ["a:40450", "b:40451"]);
    }

    #[test]
    fn partial_mismatched_and_invalid_endpoints_cannot_fall_back_to_another_cluster() {
        for (archives, reports) in [
            (Some("a:1"), None),
            (None, Some("a:1")),
            (Some("a:1"), Some("a:1,b:2")),
            (Some(""), Some("")),
            (Some("a:0"), Some("a:2")),
            (Some("a:bad"), Some("a:2")),
        ] {
            assert!(Endpoints::parse(1, archives, reports, None).is_err());
        }
        assert!(Endpoints::parse(0, None, None, None).is_err());
        assert!(Endpoints::parse(2, None, None, Some("a")).is_err());
    }
}

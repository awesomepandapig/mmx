//! End-to-end tests of the outbox, with a fake tape and a fake game. Nothing here needs Aeron.

use crate::delivery::{Cursor, Deliverer, DeliveryConfig};
use crate::fixture::{self, FakeGame, FRAME_STRIDE, Report, TestDir};
use crate::outbox::{Error, Outbox, Shared, SyncConfig};
use crate::status;
use crate::webhook;

use serde_json::Value;

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread::{self, JoinHandle, sleep};
use std::time::{Duration, Instant};

const SECRET: &[u8] = b"test-secret";

fn config(url: &str) -> DeliveryConfig {
    let mut config = DeliveryConfig::new(url.to_string(), SECRET.to_vec());
    config.first_retry = Duration::from_millis(1);
    config.max_retry = Duration::from_millis(5);
    config.timeout = Duration::from_secs(2);
    config.max_events = 7;
    config
}

/// A buffer too big to fill, so only an explicit `sync` makes frames durable.
fn manual_sync() -> SyncConfig {
    SyncConfig { max_pending_bytes: usize::MAX, max_delay: Duration::from_secs(3600) }
}

fn start_delivery(dir: &Path, shared: Arc<Shared>, config: DeliveryConfig) -> JoinHandle<()> {
    let deliverer = Deliverer::open(dir, shared, config).expect("start delivery");
    thread::spawn(move || deliverer.run().expect("delivery"))
}

fn feed(outbox: &mut Outbox, tape: &[(i64, Vec<u8>)]) {
    let resume = outbox.resume_position();
    for (position, frame) in tape.iter().filter(|(position, _)| *position > resume) {
        outbox.ingest(*position, frame).expect("ingest");
    }
    outbox.sync().expect("sync");
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
        sleep(Duration::from_millis(1));
    }
}

fn expected_events(tape: &[(i64, Vec<u8>)]) -> Vec<String> {
    let dir = TestDir::new("expected");
    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("open");
    feed(&mut outbox, tape);
    drop(outbox);
    journal_events(&dir.0)
}

/// Every event the journal in `dir` would send, in order.
fn journal_events(dir: &Path) -> Vec<String> {
    let mut reader = crate::journal::JournalReader::open(dir, 0).expect("reader");
    let mut ids = Vec::new();
    while let Some(record) = reader.next_below(std::fs::metadata(dir.join(crate::journal::JOURNAL_FILE)).expect("stat").len()).expect("read") {
        ids.extend(webhook::event(&record).map(|event| event.fill_id));
    }
    ids
}

/// Checks everything the game relies on, then applies events the way a game does.
fn assert_exactly_once_effect(events: &[Value], expected: &[String]) {
    let mut seen: HashMap<String, &Value> = HashMap::new();
    let mut first_seen = Vec::new();
    for event in events {
        let id = event["fill_id"].as_str().expect("an ID").to_string();
        match seen.get(&id) {
            Some(previous) => assert_eq!(*previous, event, "a repeat of {id} changed"),
            None => {
                seen.insert(id.clone(), event);
                first_seen.push(id);
            }
        }
    }
    assert_eq!(first_seen, *expected, "events were lost, invented or reordered");
}

#[test]
fn nothing_is_sent_before_the_disk_sync() {
    let dir = TestDir::new("sync-first");
    let game = FakeGame::start();
    let tape = fixture::tape(3);
    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("open");
    let shared = outbox.shared();
    let delivery = start_delivery(&dir.0, outbox.shared(), config(&game.url));

    for (position, frame) in &tape {
        outbox.ingest(*position, frame).expect("ingest");
    }
    sleep(Duration::from_millis(100));
    assert!(game.requests().is_empty(), "sent before the sync");
    assert_eq!(shared.position().durable_position, 0);

    outbox.sync().expect("sync");
    assert_eq!(shared.position().durable_position, tape.last().expect("a frame").0);
    let expected = expected_events(&tape);
    wait_for("delivery", || game.events().len() == expected.len());
    assert_exactly_once_effect(&game.events(), &expected);

    shared.stop();
    delivery.join().expect("join");
}

#[test]
fn frames_share_flushes_in_small_batches() {
    let dir = TestDir::new("batching");
    let tape = fixture::tape(50);
    let mut outbox = Outbox::open(&dir.0, SyncConfig { max_pending_bytes: 4096, max_delay: Duration::from_secs(3600) }).expect("open");
    feed(&mut outbox, &tape);

    let syncs = outbox.shared().syncs() as usize;
    assert!(syncs > 1 && syncs * 10 < tape.len(), "{syncs} flushes for {} frames", tape.len());
}

#[test]
fn the_delay_flushes_a_quiet_tape() {
    let dir = TestDir::new("delay");
    let tape = fixture::tape(1);
    let mut outbox = Outbox::open(&dir.0, SyncConfig { max_pending_bytes: usize::MAX, max_delay: Duration::from_millis(1) }).expect("open");
    outbox.ingest(tape[0].0, &tape[0].1).expect("ingest");
    assert_eq!(outbox.shared().position().durable_position, 0);
    sleep(Duration::from_millis(5));
    outbox.tick().expect("tick");
    assert_eq!(outbox.shared().position().durable_position, tape[0].0);
}

#[test]
fn every_attempt_is_signed_and_retries_back_off_until_accepted() {
    let dir = TestDir::new("retries");
    let game = FakeGame::start();
    game.fail_next(3);
    let tape = fixture::tape(1);
    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("open");
    let shared = outbox.shared();
    feed(&mut outbox, &tape);

    let delivery = start_delivery(&dir.0, outbox.shared(), config(&game.url));
    wait_for("the fourth attempt", || game.requests().len() == 4);
    wait_for("the failure count to clear", || shared.position().consecutive_failures == 0);

    let requests = game.requests();
    for request in &requests {
        assert_eq!(request.body, requests[0].body, "a retry changed the body");
        let timestamp: u64 = request.headers["x-mmx-timestamp"].parse().expect("a timestamp");
        assert_eq!(request.headers["x-mmx-signature"], webhook::signature(SECRET, timestamp, &request.body));
        assert_eq!(request.headers["content-type"], "application/json");
    }

    shared.stop();
    delivery.join().expect("join");
}

#[test]
fn a_redirect_is_not_a_delivery() {
    let dir = TestDir::new("redirect");
    let game = FakeGame::start();
    game.redirect_next(1);
    let tape = fixture::tape(1);
    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("open");
    let shared = outbox.shared();
    feed(&mut outbox, &tape);

    let delivery = start_delivery(&dir.0, outbox.shared(), config(&game.url));
    wait_for("the retry", || game.requests().len() >= 2);
    assert_eq!(game.requests()[0].body, game.requests()[1].body);

    shared.stop();
    delivery.join().expect("join");
}

#[test]
fn a_lost_answer_repeats_the_same_events() {
    let dir = TestDir::new("lost-answer");
    let game = FakeGame::start();
    game.lose_answer_next(1);
    let tape = fixture::tape(4);
    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("open");
    let shared = outbox.shared();
    feed(&mut outbox, &tape);

    let delivery = start_delivery(&dir.0, outbox.shared(), config(&game.url));
    let expected = expected_events(&tape);
    wait_for("delivery", || shared.position().delivered_position == tape.last().expect("a frame").0);
    let events = game.events();
    assert!(events.len() > expected.len(), "the first batch should repeat");
    assert_exactly_once_effect(&events, &expected);

    shared.stop();
    delivery.join().expect("join");
}

#[test]
fn delivery_resumes_from_its_cursor_after_a_clean_stop() {
    let dir = TestDir::new("resume");
    let game = FakeGame::start();
    let tape = fixture::tape(4);
    let first = &tape[..tape.len() / 2];

    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("open");
    let shared = outbox.shared();
    feed(&mut outbox, first);
    let delivery = start_delivery(&dir.0, outbox.shared(), config(&game.url));
    wait_for("delivery", || shared.position().delivered_position == first.last().expect("a frame").0);
    shared.stop();
    delivery.join().expect("join");
    let requests_before = game.requests().len();
    drop(outbox);

    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("reopen");
    assert_eq!(outbox.position(), first.last().expect("a frame").0);
    assert_eq!(outbox.resume_position(), first[first.len() - 2].0);
    let shared = outbox.shared();
    let delivery = start_delivery(&dir.0, outbox.shared(), config(&game.url));
    feed(&mut outbox, &tape);
    wait_for("delivery", || shared.position().delivered_position == tape.last().expect("a frame").0);

    let new_events: Vec<Value> = game.requests()[requests_before..]
        .iter()
        .flat_map(|request| serde_json::from_slice::<Value>(&request.body).expect("JSON")["events"].as_array().expect("events").clone())
        .collect();
    let new_ids: HashSet<_> = new_events.iter().map(|event| event["fill_id"].as_str().expect("an ID").to_string()).collect();
    assert_eq!(new_ids.len(), new_events.len());
    let earlier: HashSet<_> = expected_events(first).into_iter().collect();
    assert!(new_ids.is_disjoint(&earlier), "a saved cursor shouldn't repeat anything");
    assert_exactly_once_effect(&game.events(), &expected_events(&tape));

    shared.stop();
    delivery.join().expect("join");
}

#[test]
fn losing_the_cursor_repeats_events_with_the_same_ids() {
    let dir = TestDir::new("no-cursor");
    let game = FakeGame::start();
    let tape = fixture::tape(3);
    let expected = expected_events(&tape);

    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("open");
    let shared = outbox.shared();
    feed(&mut outbox, &tape);
    let delivery = start_delivery(&dir.0, outbox.shared(), config(&game.url));
    wait_for("delivery", || shared.position().delivered_position == tape.last().expect("a frame").0);
    shared.stop();
    delivery.join().expect("join");
    drop(outbox);

    std::fs::remove_file(dir.0.join("delivered")).expect("remove the cursor");
    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("reopen");
    let shared = outbox.shared();
    let delivery = start_delivery(&dir.0, outbox.shared(), config(&game.url));
    feed(&mut outbox, &tape);
    wait_for("the repeats", || game.events().len() == expected.len() * 2);
    assert_exactly_once_effect(&game.events(), &expected);

    shared.stop();
    delivery.join().expect("join");
}

#[test]
fn a_rebuilt_journal_produces_identical_events() {
    let tape = fixture::tape(5);
    let game_one = FakeGame::start();
    let game_two = FakeGame::start();
    let mut seen = Vec::new();

    for game in [&game_one, &game_two] {
        let dir = TestDir::new("rebuild");
        let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("open");
        let shared = outbox.shared();
        feed(&mut outbox, &tape);
        let mut batch_config = config(&game.url);
        batch_config.max_events = if seen.is_empty() { 1 } else { 100 };
        let delivery = start_delivery(&dir.0, outbox.shared(), batch_config);
        wait_for("delivery", || shared.position().delivered_position == tape.last().expect("a frame").0);
        shared.stop();
        delivery.join().expect("join");
        seen.push(game.events());
    }

    assert_eq!(seen[0], seen[1], "batch size or rebuild changed what the game sees");
    assert_ne!(game_one.requests().len(), game_two.requests().len());
}

#[test]
fn restart_waits_for_the_tape_to_agree_before_sending() {
    let dir = TestDir::new("verify");
    let game = FakeGame::start();
    let tape = fixture::tape(2);
    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("open");
    feed(&mut outbox, &tape);
    drop(outbox);

    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("reopen");
    let shared = outbox.shared();
    assert!(!shared.may_deliver());
    let delivery = start_delivery(&dir.0, outbox.shared(), config(&game.url));
    sleep(Duration::from_millis(100));
    assert!(game.requests().is_empty(), "sent before the tape was checked");

    let (position, frame) = tape.last().expect("a frame");
    outbox.ingest(*position, frame).expect("the matching frame");
    assert!(shared.may_deliver());
    wait_for("delivery", || !game.requests().is_empty());

    shared.stop();
    delivery.join().expect("join");
}

#[test]
fn a_tape_that_rewrote_history_stops_delivery_for_good() {
    let dir = TestDir::new("divergent");
    let game = FakeGame::start();
    let tape = fixture::tape(2);
    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("open");
    feed(&mut outbox, &tape);
    drop(outbox);

    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("reopen");
    let shared = outbox.shared();
    let delivery = start_delivery(&dir.0, outbox.shared(), config(&game.url));

    let (position, _) = tape.last().expect("a frame");
    let rewritten = fixture::report(Report { exec_id: 999, ..Report::default() });
    assert!(matches!(outbox.ingest(*position, &rewritten), Err(Error::Halted(_))));
    assert!(matches!(outbox.ingest(position + FRAME_STRIDE, &rewritten), Err(Error::Halted(_))), "halts stick");

    sleep(Duration::from_millis(50));
    assert!(game.requests().is_empty());
    assert!(!shared.may_deliver());
    assert!(shared.position().halted.expect("a reason").contains("differs"));
    assert_eq!(shared.position().durable_position, *position, "the position survives for the engine");

    shared.stop();
    delivery.join().expect("join");
}

#[test]
fn a_tape_that_skips_the_journal_end_stops_delivery() {
    let dir = TestDir::new("skipped");
    let tape = fixture::tape(2);
    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("open");
    feed(&mut outbox, &tape);
    drop(outbox);

    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("reopen");
    let beyond = tape.last().expect("a frame").0 + FRAME_STRIDE;
    assert!(matches!(outbox.ingest(beyond, &fixture::term_marker(1)), Err(Error::Halted(_))));
}

#[test]
fn an_exec_id_gap_is_never_skipped_over() {
    let dir = TestDir::new("gap");
    let tape = fixture::tape(1);
    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("open");
    feed(&mut outbox, &tape);

    let position = outbox.position();
    let gap = fixture::report(Report { exec_id: 500, ..Report::default() });
    assert!(matches!(outbox.ingest(position + FRAME_STRIDE, &gap), Err(Error::Halted(_))));
    assert_eq!(outbox.position(), position, "a refused frame isn't journaled");
}

#[test]
fn frames_offered_twice_are_journaled_once() {
    let dir = TestDir::new("twice");
    let tape = fixture::tape(1);
    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("open");
    feed(&mut outbox, &tape);
    let length = outbox.shared().durable_length();
    for (position, frame) in &tape {
        outbox.ingest(*position, frame).expect("ingest");
    }
    outbox.sync().expect("sync");
    assert_eq!(outbox.shared().durable_length(), length);
}

#[test]
fn open_orders_survive_a_restart_so_repeats_stay_flagged() {
    let dir = TestDir::new("open-orders");
    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("open");
    let new = fixture::report(Report::default());
    outbox.ingest(FRAME_STRIDE, &new).expect("ingest");
    outbox.sync().expect("sync");
    drop(outbox);

    let outbox = Outbox::open(&dir.0, manual_sync()).expect("reopen");
    assert_eq!(outbox.open_orders(), 1);
}

#[test]
fn a_journal_shorter_than_the_cursor_refuses_to_start() {
    let dir = TestDir::new("short");
    let game = FakeGame::start();
    let tape = fixture::tape(2);
    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("open");
    let shared = outbox.shared();
    feed(&mut outbox, &tape);
    let delivery = start_delivery(&dir.0, outbox.shared(), config(&game.url));
    wait_for("delivery", || shared.position().delivered_position == tape.last().expect("a frame").0);
    shared.stop();
    delivery.join().expect("join");
    drop(outbox);
    assert_ne!(Cursor::load(&dir.0).expect("cursor"), Cursor::default());

    // The disk lost synced records that delivery had already passed.
    let path = dir.0.join(crate::journal::JOURNAL_FILE);
    let length = std::fs::metadata(&path).expect("stat").len();
    std::fs::OpenOptions::new().write(true).open(&path).expect("open").set_len(length / 2).expect("cut");

    let outbox = Outbox::open(&dir.0, manual_sync()).expect("the journal itself opens");
    assert!(Deliverer::open(&dir.0, outbox.shared(), config(&game.url)).is_err());
}

fn http_get(address: std::net::SocketAddr, target: &str) -> (String, Vec<u8>) {
    let mut stream = TcpStream::connect(address).expect("connect");
    write!(stream, "GET {target} HTTP/1.1\r\nHost: outbox\r\n\r\n").expect("write");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read");
    let split = response.windows(4).position(|window| window == b"\r\n\r\n").expect("a head");
    (String::from_utf8_lossy(&response[..split]).to_string(), response[split + 4..].to_vec())
}

#[test]
fn the_status_endpoint_reports_the_durable_position_and_serves_the_tail() {
    let dir = TestDir::new("status");
    let tape = fixture::tape(1);
    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("open");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let address = listener.local_addr().expect("address");
    let (status_dir, status_shared) = (dir.0.clone(), outbox.shared());
    let webhook = status::WebhookStatus { configured: true, ..Default::default() };
    thread::spawn(move || status::serve(&listener, status_dir, status_shared, webhook));

    // Undurable frames must not count.
    for (position, frame) in &tape {
        outbox.ingest(*position, frame).expect("ingest");
    }
    let (_, body) = http_get(address, "/position");
    let position: Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(position["durable_position"], 0);

    outbox.sync().expect("sync");
    let (head, body) = http_get(address, "/position");
    assert!(head.starts_with("HTTP/1.1 200"));
    let position: Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(position["durable_position"], tape.last().expect("a frame").0);
    assert_eq!(position["durable_exec_id"], 8);
    assert_eq!(position["halted"], Value::Null);

    assert!(http_get(address, "/healthz").0.starts_with("HTTP/1.1 200"));
    assert!(http_get(address, "/readyz").0.starts_with("HTTP/1.1 200"));
    assert!(http_get(address, "/nothing").0.starts_with("HTTP/1.1 404"));

    // The tail after the second to last frame is just the last record.
    let (head, tail) = http_get(address, &format!("/journal?from={}", tape[tape.len() - 2].0));
    assert!(head.contains("application/octet-stream"));
    let mut expected = Vec::new();
    let (position, frame) = tape.last().expect("a frame");
    crate::journal::Record { position: *position, exec_id: 8, flags: 0, frame: frame.clone() }.encode_into(&mut expected);
    assert_eq!(tail, expected);
}

const CHILD_DIR: &str = "OUTBOX_TEST_CHILD_DIR";
const CHILD_URL: &str = "OUTBOX_TEST_CHILD_URL";
const CRASH_ROUNDS: u64 = 400;

/// Not a test by itself. The tests below run this in its own process, and kill it.
#[test]
#[ignore = "run by the tests below, in its own process"]
fn crash_child() {
    let (Ok(dir), Ok(url)) = (std::env::var(CHILD_DIR), std::env::var(CHILD_URL)) else { return };
    // Delivery never returns here, so don't outlive a test binary that was killed itself.
    let parent = std::os::unix::process::parent_id();
    thread::spawn(move || {
        while std::os::unix::process::parent_id() == parent {
            sleep(Duration::from_millis(50));
        }
        std::process::exit(1);
    });

    let dir = std::path::PathBuf::from(dir);
    let mut outbox = Outbox::open(&dir, SyncConfig { max_pending_bytes: 1024, max_delay: Duration::from_millis(1) }).expect("open");
    let mut delivery_config = config(&url);
    delivery_config.max_events = 3;
    let delivery = start_delivery(&dir, outbox.shared(), delivery_config);

    let tape = fixture::tape(CRASH_ROUNDS);
    let resume = outbox.resume_position();
    for (position, frame) in tape.iter().filter(|(position, _)| *position > resume) {
        outbox.ingest(*position, frame).expect("ingest");
        outbox.tick().expect("tick");
    }
    outbox.sync().expect("sync");
    delivery.join().expect("delivery");
}

/// Kills the child when dropped, so a failed test can't leave it running.
struct KilledOnDrop(Child);

impl Drop for KilledOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_child(dir: &Path, game: &FakeGame) -> KilledOnDrop {
    KilledOnDrop(
        Command::new(std::env::current_exe().expect("this test binary"))
            .args(["--exact", "recovery_tests::crash_child", "--ignored", "--nocapture"])
            .env(CHILD_DIR, dir)
            .env(CHILD_URL, &game.url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the child"),
    )
}

fn kill(child: KilledOnDrop) {
    drop(child);
}

fn distinct_events(game: &FakeGame) -> usize {
    game.events().iter().map(|event| event["fill_id"].as_str().expect("an ID").to_string()).collect::<HashSet<_>>().len()
}

/// Kills a real process with SIGKILL, over and over, while it's journaling and sending.
#[test]
fn a_killed_process_loses_nothing_and_never_changes_an_id() {
    let dir = TestDir::new("killed");
    let game = FakeGame::start();
    let tape = fixture::tape(CRASH_ROUNDS);
    let expected = expected_events(&tape);

    let mut target = expected.len() / 8;
    for _ in 0..4 {
        let child = spawn_child(&dir.0, &game);
        wait_for("progress before the kill", || game.events().len() >= target);
        kill(child);
        target += expected.len() / 8;
    }

    let child = spawn_child(&dir.0, &game);
    wait_for("every event", || distinct_events(&game) >= expected.len());
    kill(child);

    assert_exactly_once_effect(&game.events(), &expected);
    assert_eq!(journal_events(&dir.0), expected, "the journal differs from the tape");
}

/// The game refuses a batch and the sender dies while retrying it, so only the journal and the
/// cursor can bring those events back.
#[test]
fn a_kill_during_a_game_outage_loses_nothing() {
    let dir = TestDir::new("outage");
    let game = FakeGame::start();
    let tape = fixture::tape(CRASH_ROUNDS);
    let expected = expected_events(&tape);

    let child = spawn_child(&dir.0, &game);
    wait_for("some progress", || game.events().len() >= 30);
    game.fail_next(usize::MAX);
    let requests = game.requests().len();
    wait_for("retries against the outage", || game.requests().len() >= requests + 5);
    kill(child);

    game.fail_next(0);
    let child = spawn_child(&dir.0, &game);
    wait_for("every event", || distinct_events(&game) >= expected.len());
    kill(child);

    assert_exactly_once_effect(&game.events(), &expected);
}

/// Replays what a game would do, including repeats, and checks that all escrow clears exactly once.
#[test]
fn a_game_that_dedupes_ends_with_no_stale_escrow() {
    let dir = TestDir::new("escrow");
    let game = FakeGame::start();
    game.lose_answer_next(2);
    let tape = fixture::tape(20);
    let mut outbox = Outbox::open(&dir.0, manual_sync()).expect("open");
    let shared = outbox.shared();
    feed(&mut outbox, &tape);
    let delivery = start_delivery(&dir.0, outbox.shared(), config(&game.url));
    wait_for("delivery", || shared.position().delivered_position == tape.last().expect("a frame").0);
    shared.stop();
    delivery.join().expect("join");

    let mut applied = HashSet::new();
    let mut filled: HashMap<String, f64> = HashMap::new();
    let mut released: HashMap<String, f64> = HashMap::new();
    let mut closed = HashSet::new();
    let number = |value: &Value| value.as_str().expect("a decimal string").parse::<f64>().expect("a number");
    for event in game.events() {
        if !applied.insert(event["fill_id"].as_str().expect("an ID").to_string()) {
            continue;
        }
        let order = event["client_order_id"].as_str().expect("an order").to_string();
        if event["type"] == "fill" {
            *filled.entry(order.clone()).or_default() += number(&event["last_qty"]);
        }
        *released.entry(order.clone()).or_default() += number(&event["release_qty"]);
        if event["terminal"] == true {
            assert!(closed.insert(order.clone()), "{order} closed twice");
            // Every order in this tape is held for its own `order_qty`.
            let held = number(&event["order_qty"]);
            let accounted = filled.get(&order).copied().unwrap_or(0.0) + released.get(&order).copied().unwrap_or(0.0);
            assert_eq!(accounted, held, "{order} left escrow behind");
        }
    }
    // Two sides and one rejected order, per round. Cancel rejects close nothing.
    assert_eq!(closed.len(), 3 * 20);
}

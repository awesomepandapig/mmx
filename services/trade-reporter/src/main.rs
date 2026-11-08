mod rows;
mod store;

use std::env;
use std::hint::spin_loop;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, Instant};

use log::{error, info, warn};

use tape::candle::CandleBook;
use tape::follower::TapeConfig;
use tape::frame::{self, Frame};
use tape::source::Source;

use store::Store;

const FRAGMENT_LIMIT: usize = 1024;
const REPLAY_STREAM: i32 = 3030;
const FLUSH_ROWS: usize = 10_000;
const FLUSH_INTERVAL: Duration = Duration::from_millis(100);
const RETRY_AFTER: Duration = Duration::from_secs(1);

/// Sends everything queued. While QuestDB is away, reading stops and this service falls behind
/// the tape, which is expected.
fn flush_until_done(store: &mut Store, candles: &mut CandleBook, position: Option<i64>) {
    while let Err(e) = store.flush(candles, position) {
        error!("QuestDB: Failed to flush: {}", e);
        sleep(RETRY_AFTER);
    }
}

fn main() -> ExitCode {
    env_logger::init();

    let start_time = Instant::now();
    let rebuild = env::args().any(|argument| argument == "--rebuild");

    let config = match TapeConfig::from_env(REPLAY_STREAM) {
        Ok(config) => config,
        Err(e) => {
            error!("{}", e);
            return ExitCode::FAILURE;
        }
    };
    if rebuild && config.is_none() {
        error!("--rebuild needs a tape to read, so set TAPE_ARCHIVES and TAPE_REPORTS");
        return ExitCode::FAILURE;
    }

    let questdb_conf = transport::env_or("QUESTDB_CONF", "http::addr=localhost:9000;");
    let mut store = match Store::open(&questdb_conf) {
        Ok(store) => store,
        Err(e) => {
            error!("QuestDB: {}", e);
            return ExitCode::FAILURE;
        }
    };
    store.prepare(rebuild);

    // A standalone engine records nothing and restarts its IDs, so there's nothing to resume.
    let (position, mut candles) = match config {
        Some(_) => store.resume(),
        None => (0, CandleBook::default()),
    };
    let mut source = Source::open(config, position);

    let startup_duration = start_time.elapsed();
    info!(
        "Startup complete in {:.2?} — ready to accept requests",
        startup_duration
    );

    let mut flushed_position = source.position();
    let mut last_flush = Instant::now();

    loop {
        let fragments_read = source.poll(
            FRAGMENT_LIMIT,
            &mut |buffer, _position| match frame::parse(buffer) {
                Frame::Report(report) => {
                    store.record(&report);
                    candles.apply(&report);
                }
                Frame::CancelReject | Frame::TermMarker(_) => {}
                Frame::Unknown(template_id) => warn!("Skipping unknown template {}", template_id),
                Frame::Malformed => warn!("Skipping a malformed message of {} bytes", buffer.len()),
            },
        );

        let position = source.position();
        let idle_flush_due = last_flush.elapsed() >= FLUSH_INTERVAL;
        let has_news = store.queued_rows() > 0 || position != flushed_position;
        if store.queued_rows() >= FLUSH_ROWS || (idle_flush_due && has_news) {
            flush_until_done(&mut store, &mut candles, position);
            flushed_position = position;
            last_flush = Instant::now();
        }

        if fragments_read == 0 {
            spin_loop();
        }
    }
}

//! Keeps QuestDB up to date with the tape.
//!
//! QuestDB is only a view of the tape, so its tables hold their own resume point, which vanishes
//! with them: losing QuestDB means replaying the tape from the start, with no other steps. A row
//! that's written twice replaces itself, so replaying is always safe.

use std::thread::sleep;
use std::time::{Duration, Instant};

use log::{error, info, warn};

use questdb::ingress::{Buffer, Sender};

use sbe::exec_type_enum::ExecTypeEnum;

use serde_json::Value;

use tape::candle::{Candle, CandleBook};
use tape::messages::{ExecutionReportMessage, symbol_bytes};

use crate::rows::{write_candle, write_order, write_position, write_trade};

const TABLES: [&str; 4] = ["orders", "trades", "candles", "recorder_position"];

const CREATE_TABLES: [&str; 4] = [
    "CREATE TABLE IF NOT EXISTS orders (
        ts TIMESTAMP, symbol SYMBOL, exec_type SYMBOL, ord_status SYMBOL, side SYMBOL,
        ord_rej_reason SYMBOL, account UUID, cl_ord_id UUID, order_id LONG, exec_id LONG,
        price LONG, ord_qty LONG, leaves_qty LONG, cum_qty LONG, avg_px LONG
    ) TIMESTAMP(ts) PARTITION BY DAY WAL DEDUP UPSERT KEYS(ts, exec_id)",
    "CREATE TABLE IF NOT EXISTS trades (
        ts TIMESTAMP, symbol SYMBOL, side SYMBOL, account UUID, cl_ord_id UUID, match_id LONG,
        order_id LONG, exec_id LONG, price LONG, quantity LONG
    ) TIMESTAMP(ts) PARTITION BY DAY WAL DEDUP UPSERT KEYS(ts, exec_id)",
    "CREATE TABLE IF NOT EXISTS candles (
        ts TIMESTAMP, symbol SYMBOL, open LONG, high LONG, low LONG, close LONG, volume LONG,
        trades LONG, last_match_id LONG
    ) TIMESTAMP(ts) PARTITION BY MONTH WAL DEDUP UPSERT KEYS(ts, symbol)",
    "CREATE TABLE IF NOT EXISTS recorder_position (
        ts TIMESTAMP, position LONG, updated_at TIMESTAMP
    ) TIMESTAMP(ts) PARTITION BY YEAR WAL DEDUP UPSERT KEYS(ts)",
];

const SAVED_POSITION: &str = "SELECT position FROM recorder_position";
const SAVED_CANDLES: &str = "SELECT symbol, cast(ts AS long), open, high, low, close, volume, trades, last_match_id \
     FROM candles LATEST ON ts PARTITION BY symbol";
const WAL_STATE: &str = "SELECT name, writerTxn, sequencerTxn FROM wal_tables()";

const MICROS_PER_SECOND: u64 = 1_000_000;
const RETRY_AFTER: Duration = Duration::from_secs(1);
const SQL_TIMEOUT: Duration = Duration::from_secs(5);
/// Writes become visible a moment after they're accepted, and each table applies them separately.
const WAL_WAIT: Duration = Duration::from_secs(10);

/// The HTTP address in a QuestDB config string, which is also where SQL goes.
fn http_base(conf: &str) -> Result<String, String> {
    let (scheme, parameters) = conf
        .split_once("::")
        .ok_or_else(|| format!("QUESTDB_CONF isn't a config string: {conf}"))?;
    if scheme != "http" && scheme != "https" {
        return Err(format!("QUESTDB_CONF must use http or https, not {scheme}"));
    }
    let addr = parameters
        .split(';')
        .find_map(|parameter| parameter.strip_prefix("addr="))
        .ok_or_else(|| format!("QUESTDB_CONF has no addr: {conf}"))?;
    Ok(format!("{scheme}://{addr}"))
}

fn sql_response(body: &str) -> Result<Value, String> {
    let result: Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    if !result.is_object() {
        return Err("QuestDB returned a non-object SQL response".to_string());
    }
    if let Some(error) = result.get("error").filter(|error| !error.is_null()) {
        return Err(format!("QuestDB SQL error: {error}"));
    }
    Ok(result)
}

fn dataset(result: &Value) -> Result<&Vec<Value>, String> {
    result["dataset"]
        .as_array()
        .ok_or_else(|| "QuestDB SQL response has no dataset".to_string())
}

fn saved_position(result: &Value) -> Result<i64, String> {
    let rows = dataset(result)?;
    if rows.is_empty() {
        return Ok(0);
    }
    if rows.len() != 1 || rows[0].as_array().is_none_or(|row| row.len() != 1) {
        return Err("Invalid saved recorder position rows".to_string());
    }
    rows[0][0]
        .as_i64()
        .filter(|position| *position >= 0)
        .ok_or_else(|| "Invalid saved recorder position".to_string())
}

fn saved_candles(result: &Value) -> Result<Vec<Candle>, String> {
    let mut candles = Vec::new();
    let mut symbols = std::collections::HashSet::new();
    for row in dataset(result)? {
        let candle = saved_candle(row).ok_or_else(|| "Invalid saved candle row".to_string())?;
        if !symbols.insert(candle.symbol) {
            return Err("Duplicate saved candle symbol".to_string());
        }
        candles.push(candle);
    }
    Ok(candles)
}

fn saved_candle(row: &Value) -> Option<Candle> {
    if row.as_array()?.len() != 9 {
        return None;
    }
    Some(Candle {
        symbol: symbol_bytes(row[0].as_str()?)?,
        start: u64::try_from(row[1].as_i64()?).ok()? / MICROS_PER_SECOND,
        open: row[2].as_i64()?,
        high: row[3].as_i64()?,
        low: row[4].as_i64()?,
        close: row[5].as_i64()?,
        volume: row[6].as_i64()?,
        trades: u64::try_from(row[7].as_i64()?).ok()?,
        last_match_id: u64::try_from(row[8].as_i64()?).ok()?,
    })
}

/// Every expected table must be present exactly once with valid, fully applied WAL metadata.
fn wal_applied(result: &Value) -> bool {
    let Ok(rows) = dataset(result) else {
        return false;
    };
    let mut seen = [false; TABLES.len()];
    for row in rows {
        let Some(index) = row[0]
            .as_str()
            .and_then(|name| TABLES.iter().position(|table| *table == name))
        else {
            continue;
        };
        let (Some(writer), Some(sequencer)) = (row[1].as_i64(), row[2].as_i64()) else {
            return false;
        };
        if seen[index] || writer < 0 || writer != sequencer {
            return false;
        }
        seen[index] = true;
    }
    seen.into_iter().all(|present| present)
}

/// Never read resume state until the independent WAL tables have all applied their accepted data.
/// The caller retries this entire attempt on failure, including rechecking the barrier.
fn await_applied(
    query: &mut impl FnMut(&str) -> Result<Value, String>,
    now: &mut impl FnMut() -> Duration,
    wait: &mut impl FnMut(Duration),
) -> Result<(), String> {
    let started = now();
    loop {
        if now().saturating_sub(started) >= WAL_WAIT {
            return Err("Timed out waiting for all recorder WAL tables to apply".to_string());
        }
        if wal_applied(&query(WAL_STATE)?) {
            break;
        }
        wait(Duration::from_millis(100));
    }
    Ok(())
}

fn resume_attempt(
    query: &mut impl FnMut(&str) -> Result<Value, String>,
    now: &mut impl FnMut() -> Duration,
    wait: &mut impl FnMut(Duration),
) -> Result<(i64, CandleBook), String> {
    await_applied(query, now, wait)?;
    let position = saved_position(&query(SAVED_POSITION)?)?;
    if position == 0 {
        // Data may have been accepted before the first cursor. Replaying from zero dedupes it.
        return Ok((0, CandleBook::default()));
    }
    // A prior process's outstanding cursor request might be accepted after the first barrier.
    // Its data flush was accepted before that cursor was sent. Recheck after observing the cursor
    // so the associated candle writes cannot still be waiting for WAL application.
    await_applied(query, now, wait)?;
    let candles = saved_candles(&query(SAVED_CANDLES)?)?;
    Ok((position, CandleBook::resume(candles)))
}

fn resume_until_done(
    mut query: impl FnMut(&str) -> Result<Value, String>,
    mut now: impl FnMut() -> Duration,
    mut wait: impl FnMut(Duration),
) -> (i64, CandleBook) {
    loop {
        match resume_attempt(&mut query, &mut now, &mut wait) {
            Ok(state) => return state,
            Err(error) => {
                warn!("QuestDB: Resume is blocked; retrying: {error}");
                wait(RETRY_AFTER);
            }
        }
    }
}

pub struct Store {
    sender: Sender,
    rows: Buffer,
    position_row: Buffer,
    sql_url: String,
    agent: ureq::Agent,
}

impl Store {
    pub fn open(conf: &str) -> Result<Self, String> {
        let sql_url = format!("{}/exec", http_base(conf)?);
        let sender = Sender::from_conf(conf).map_err(|e| e.to_string())?;
        Ok(Self {
            sender,
            rows: Buffer::new(),
            position_row: Buffer::new(),
            sql_url,
            agent: ureq::AgentBuilder::new().timeout(SQL_TIMEOUT).build(),
        })
    }

    fn sql(&self, query: &str) -> Result<Value, String> {
        let response = self.agent.get(&self.sql_url).query("query", query).call();
        let body = match response {
            Ok(response) => response.into_string().map_err(|e| e.to_string())?,
            Err(ureq::Error::Status(code, response)) => {
                let body = response.into_string().unwrap_or_default();
                return Err(format!("{code}: {body}"));
            }
            Err(e) => return Err(e.to_string()),
        };
        sql_response(&body)
    }

    /// Runs a query, waiting for as long as QuestDB takes to come back.
    fn sql_until_done(&self, query: &str) -> Value {
        loop {
            match self.sql(query) {
                Ok(result) => return result,
                Err(e) => {
                    warn!("QuestDB: Retrying a query: {}", e);
                    sleep(RETRY_AFTER);
                }
            }
        }
    }

    /// Creates the tables, which need `DEDUP` so that replays are safe. Dropping them first
    /// starts a rebuild.
    pub fn prepare(&self, rebuild: bool) {
        if rebuild {
            for table in TABLES {
                info!("Dropping {}", table);
                self.sql_until_done(&format!("DROP TABLE IF EXISTS {table}"));
            }
        }
        for create in CREATE_TABLES {
            self.sql_until_done(create);
        }
    }

    /// Waits for a verified WAL barrier and valid saved state before following the tape.
    /// SQL failures, stalled WAL application and malformed checkpoints keep startup blocked.
    /// This assumes one recorder writer and no concurrent external rebuild.
    pub fn resume(&self) -> (i64, CandleBook) {
        let started = Instant::now();
        let (position, candles) =
            resume_until_done(|query| self.sql(query), || started.elapsed(), sleep);
        info!("Resuming from verified recorder position {position}");
        (position, candles)
    }

    /// Queues a report's rows. Nothing goes out until `flush`.
    pub fn record(&mut self, report: &ExecutionReportMessage) {
        if let Err(e) = self.queue(report) {
            error!("Skipping report {}: {}", report.exec_id, e);
            if let Err(e) = self.rows.rewind_to_marker() {
                error!("Failed to discard a partial row: {}", e);
            }
        }
    }

    fn queue(&mut self, report: &ExecutionReportMessage) -> questdb::Result<()> {
        self.rows.set_marker()?;
        write_order(&mut self.rows, report)?;
        if report.exec_type == ExecTypeEnum::Trade {
            write_trade(&mut self.rows, report)?;
        }
        Ok(())
    }

    pub fn queued_rows(&self) -> usize {
        self.rows.row_count()
    }

    /// Sends everything queued, then the position it reaches. That order means a crash in between
    /// leaves the position behind the data, which only repeats rows. After a failure the rows stay
    /// queued, so call this again.
    pub fn flush(&mut self, candles: &mut CandleBook, position: Option<i64>) -> Result<(), String> {
        for candle in candles.take_changed() {
            if let Err(e) = write_candle(&mut self.rows, &candle) {
                error!("Skipping a candle: {}", e);
            }
        }
        // A term marker moves the position with no rows, and the client refuses an empty buffer.
        if self.rows.row_count() > 0 {
            self.sender
                .flush(&mut self.rows)
                .map_err(|e| e.to_string())?;
        }

        if let Some(position) = position {
            self.position_row.clear();
            write_position(&mut self.position_row, position).map_err(|e| e.to_string())?;
            self.sender
                .flush(&mut self.position_row)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    use tape::messages::sample_report;

    fn store() -> Store {
        Store::open("http::addr=localhost:1;").expect("opening doesn't connect")
    }

    #[test]
    fn a_trade_queues_an_order_row_and_a_trade_row() {
        let mut store = store();
        store.record(&sample_report());
        assert_eq!(store.queued_rows(), 1);

        let trade = ExecutionReportMessage {
            exec_type: ExecTypeEnum::Trade,
            ..sample_report()
        };
        store.record(&trade);
        assert_eq!(store.queued_rows(), 3);
    }

    #[test]
    fn a_report_that_cannot_be_written_leaves_no_partial_row() {
        let mut store = store();
        store.record(&sample_report());
        store.record(&ExecutionReportMessage {
            transact_time: u64::MAX,
            ..sample_report()
        });
        assert_eq!(store.queued_rows(), 1);

        store.record(&sample_report());
        assert_eq!(store.queued_rows(), 2);
        assert_eq!(store.rows.as_str().lines().count(), 2);
    }

    #[test]
    fn a_failed_flush_keeps_the_rows() {
        let mut store = Store::open("http::addr=localhost:1;retry_timeout=0;").expect("opens");
        store.record(&sample_report());

        assert!(store.flush(&mut CandleBook::default(), Some(1)).is_err());
        assert_eq!(store.queued_rows(), 1);
    }

    #[test]
    fn a_flush_with_no_rows_succeeds() {
        let mut store = store();
        assert_eq!(store.flush(&mut CandleBook::default(), None), Ok(()));
    }

    #[test]
    fn finds_the_http_address() {
        assert_eq!(
            http_base("http::addr=localhost:9000;").as_deref(),
            Ok("http://localhost:9000")
        );
        assert_eq!(
            http_base("https::retry_timeout=1000;addr=db:9000;").as_deref(),
            Ok("https://db:9000")
        );
    }

    #[test]
    fn refuses_configs_without_http() {
        assert!(http_base("tcp::addr=localhost:9009;").is_err());
        assert!(http_base("http::retry_timeout=1000;").is_err());
        assert!(http_base("localhost:9000").is_err());
    }

    #[test]
    fn every_table_is_created_with_dedup() {
        assert_eq!(CREATE_TABLES.len(), TABLES.len());
        for (table, create) in TABLES.iter().zip(CREATE_TABLES) {
            assert!(create.contains(&format!("EXISTS {table} (")), "{create}");
            assert!(create.contains("WAL DEDUP UPSERT KEYS(ts"), "{create}");
        }
    }

    #[test]
    fn reads_a_saved_position() {
        assert_eq!(saved_position(&json!({"dataset": [[4096]]})), Ok(4096));
        assert_eq!(saved_position(&json!({"dataset": []})), Ok(0));
        for invalid in [
            json!({}),
            json!({"dataset": [[null]]}),
            json!({"dataset": [[-1]]}),
            json!({"dataset": [[1], [2]]}),
        ] {
            assert!(saved_position(&invalid).is_err());
        }
    }

    #[test]
    fn reads_saved_candles() {
        let result = json!({"dataset": [
            ["ITEM", 300_000_000i64, 1, 4, 1, 2, 5, 2, 7],
            ["OTHER", 600_000_000i64, 9, 9, 9, 9, 1, 1, 8],
        ]});

        let candles = saved_candles(&result).expect("valid candles");

        assert_eq!(candles.len(), 2);
        assert_eq!(candles[0].symbol, *b"ITEM\0\0");
        assert_eq!(candles[0].start, 300);
        assert_eq!(candles[0].close, 2);
        assert_eq!(candles[0].last_match_id, 7);
        assert_eq!(candles[1].start, 600);
    }

    #[test]
    fn unreadable_saved_candles_block_resume() {
        for result in [
            json!({"dataset": [["TOOLONGNAME", 0, 1, 1, 1, 1, 1, 1, 1]]}),
            json!({"dataset": [["ITEM", null]]}),
            json!({}),
        ] {
            assert!(saved_candles(&result).is_err());
        }
        assert!(
            saved_candles(&json!({"dataset": []}))
                .expect("no candles yet")
                .is_empty()
        );
    }

    fn applied() -> Value {
        json!({"dataset": [
            ["orders", 5, 5], ["trades", 2, 2],
            ["candles", 2, 2], ["recorder_position", 1, 1],
            ["unrelated", 1, 9],
        ]})
    }

    #[test]
    fn wal_barrier_requires_every_table_and_sane_transactions() {
        assert!(wal_applied(&applied()));
        for invalid in [
            json!({"dataset": []}),
            json!({"dataset": [["orders", 5, 5]]}),
            json!({"error": "nope"}),
        ] {
            assert!(!wal_applied(&invalid));
        }
        for transactions in [
            json!([2, 3]),
            json!([null, 2]),
            json!([-1, -1]),
            json!([3, 2]),
            json!(["2", "2"]),
        ] {
            let mut result = applied();
            result["dataset"][2][1] = transactions[0].clone();
            result["dataset"][2][2] = transactions[1].clone();
            assert!(!wal_applied(&result));
        }
        let mut duplicate = applied();
        duplicate["dataset"]
            .as_array_mut()
            .expect("rows")
            .push(json!(["orders", 5, 5]));
        assert!(!wal_applied(&duplicate));
    }

    #[test]
    fn sql_errors_are_errors_even_in_successful_http_bodies() {
        assert!(sql_response(r#"{"error":"invalid query","position":0}"#).is_err());
        assert!(sql_response("[]").is_err());
        assert!(sql_response("not json").is_err());
        assert!(sql_response(r#"{"ddl":"OK"}"#).is_ok());
        assert!(sql_response(r#"{"dataset":[]}"#).is_ok());
    }

    #[test]
    fn resume_retries_error_timeout_and_missing_tables_before_reading_state() {
        use std::cell::Cell;
        use std::collections::VecDeque;

        let mut replies = VecDeque::from([
            (WAL_STATE, Err("WAL query unavailable".to_string())),
            (WAL_STATE, Ok(json!({"dataset": [["orders", 5, 6]]}))),
            (WAL_STATE, Ok(json!({"dataset": []}))),
            (WAL_STATE, Ok(applied())),
            (SAVED_POSITION, Ok(json!({"dataset": [[4096]]}))),
            (WAL_STATE, Ok(applied())),
            (
                SAVED_CANDLES,
                Ok(json!({"dataset": [["ITEM", 300_000_000i64, 1, 4, 1, 2, 5, 2, 7]]})),
            ),
        ]);
        let clock = Cell::new(Duration::ZERO);
        let mut blocked_polls = 0;
        let (position, _) = resume_until_done(
            |query| {
                let (expected, reply) = replies.pop_front().expect("unexpected query");
                assert_eq!(
                    query, expected,
                    "state must not be read before verified WAL"
                );
                reply
            },
            || clock.get(),
            |delay| {
                // A fake clock reaches the attempt deadline without any real sleeping.
                if delay == Duration::from_millis(100) {
                    blocked_polls += 1;
                    clock.set(clock.get() + WAL_WAIT);
                } else {
                    clock.set(clock.get() + delay);
                }
            },
        );
        assert_eq!(position, 4096);
        assert_eq!(blocked_polls, 2);
        assert!(replies.is_empty());
    }

    #[test]
    fn malformed_durable_state_rechecks_barrier_instead_of_becoming_empty() {
        use std::collections::VecDeque;
        let mut replies = VecDeque::from([
            (WAL_STATE, Ok(applied())),
            (SAVED_POSITION, Ok(json!({"dataset": [[null]]}))),
            (WAL_STATE, Ok(applied())),
            (SAVED_POSITION, Ok(json!({"dataset": [[4096]]}))),
            (WAL_STATE, Ok(applied())),
            (SAVED_CANDLES, Ok(json!({"dataset": [["ITEM", null]]}))),
            (WAL_STATE, Ok(applied())),
            (SAVED_POSITION, Ok(json!({"dataset": [[4096]]}))),
            (WAL_STATE, Ok(applied())),
            (SAVED_CANDLES, Ok(json!({"dataset": []}))),
        ]);
        let mut waits = 0;
        let (position, _) = resume_until_done(
            |query| {
                let (expected, reply) = replies.pop_front().expect("unexpected query");
                assert_eq!(query, expected);
                reply
            },
            || Duration::ZERO,
            |_| waits += 1,
        );
        assert_eq!(position, 4096);
        assert_eq!(waits, 2);
        assert!(replies.is_empty());
    }

    #[test]
    fn accepted_candle_ahead_of_cursor_replays_without_counting_it_twice() {
        use sbe::side_enum::SideEnum;
        use std::collections::VecDeque;
        // Data through match 2 was accepted before a crash, but the cursor still covers match 1.
        let mut replies = VecDeque::from([
            (WAL_STATE, Ok(applied())),
            (SAVED_POSITION, Ok(json!({"dataset": [[4096]]}))),
            (WAL_STATE, Ok(applied())),
            (
                SAVED_CANDLES,
                Ok(json!({"dataset": [["ITEM", 300_000_000i64, 100, 100, 100, 100, 5, 2, 2]]})),
            ),
        ]);
        let (_, mut candles) = resume_until_done(
            |query| {
                let (expected, reply) = replies.pop_front().expect("unexpected query");
                assert_eq!(query, expected);
                reply
            },
            || Duration::ZERO,
            |_| panic!("valid applied state must not wait"),
        );
        let trade = |match_id, quantity| ExecutionReportMessage {
            symbol: *b"ITEM\0\0",
            exec_type: ExecTypeEnum::Trade,
            side: SideEnum::Buy,
            transact_time: 301_000_000_000,
            trd_match_id: Some(match_id),
            last_px: 100,
            last_qty: quantity,
            ..sample_report()
        };
        candles.apply(&trade(2, 3));
        assert!(
            candles.take_changed().is_empty(),
            "accepted replay already counted"
        );
        candles.apply(&trade(3, 2));
        let rows = candles.take_changed();
        assert_eq!(rows[0].volume, 7);
        assert_eq!(rows[0].trades, 3);
        assert!(replies.is_empty());
    }

    #[test]
    fn cursor_observed_after_initial_barrier_requires_another_barrier_before_candles() {
        use std::collections::VecDeque;
        let mut replies = VecDeque::from([
            (WAL_STATE, Ok(applied())),
            (SAVED_POSITION, Ok(json!({"dataset": [[4096]]}))),
            (WAL_STATE, Ok(json!({"dataset": [["candles", 1, 2]]}))),
            (WAL_STATE, Ok(applied())),
            (SAVED_CANDLES, Ok(json!({"dataset": []}))),
        ]);
        let mut waits = 0;
        let (position, _) = resume_until_done(
            |query| {
                let (expected, reply) = replies.pop_front().expect("unexpected query");
                assert_eq!(
                    query, expected,
                    "candles must wait for the observed cursor's data"
                );
                reply
            },
            || Duration::ZERO,
            |delay| {
                assert_eq!(delay, Duration::from_millis(100));
                waits += 1;
            },
        );
        assert_eq!(position, 4096);
        assert_eq!(waits, 1);
        assert!(replies.is_empty());
    }
    #[test]
    fn accepted_data_without_any_cursor_replays_from_zero() {
        let mut calls = Vec::new();
        let (position, mut candles) = resume_until_done(
            |query| {
                calls.push(query.to_string());
                match query {
                    WAL_STATE => Ok(applied()),
                    SAVED_POSITION => Ok(json!({"dataset": []})),
                    _ => panic!("a missing cursor requires full replay, not saved candle state"),
                }
            },
            || Duration::ZERO,
            |_| panic!("valid applied state must not wait"),
        );
        assert_eq!(position, 0);
        assert!(candles.take_changed().is_empty());
        assert_eq!(calls, [WAL_STATE, SAVED_POSITION]);
    }
}

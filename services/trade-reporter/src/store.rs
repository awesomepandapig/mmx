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

/// The first column of the first row of a query's result.
fn first_long(result: &Value) -> Option<i64> {
    result["dataset"][0][0].as_i64()
}

fn saved_candles(result: &Value) -> Vec<Candle> {
    let Some(rows) = result["dataset"].as_array() else { return Vec::new() };
    rows.iter().filter_map(saved_candle).collect()
}

fn saved_candle(row: &Value) -> Option<Candle> {
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

/// Whether every one of our tables has applied everything it accepted.
fn wal_applied(result: &Value) -> bool {
    let Some(rows) = result["dataset"].as_array() else { return false };
    rows.iter()
        .filter(|row| row[0].as_str().is_some_and(|name| TABLES.contains(&name)))
        .all(|row| row[1].as_i64().is_some() && row[1] == row[2])
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
        serde_json::from_str(&body).map_err(|e| e.to_string())
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

    /// Waits until saved state is visible. Tables apply writes independently, so a position can
    /// otherwise be seen before the candles written ahead of it.
    fn await_applied(&self) {
        let started = Instant::now();
        while started.elapsed() < WAL_WAIT {
            match self.sql(WAL_STATE) {
                Ok(result) if wal_applied(&result) => return,
                Ok(_) => sleep(Duration::from_millis(100)),
                Err(e) => {
                    warn!("QuestDB: Failed to read the table state: {}", e);
                    return;
                }
            }
        }
        warn!("QuestDB: Tables are still applying writes, so candles may be behind");
    }

    /// The position to resume from and the candles open at that point. With no position,
    /// everything is rebuilt from the tape.
    pub fn resume(&self) -> (i64, CandleBook) {
        self.await_applied();
        let position = first_long(&self.sql_until_done(SAVED_POSITION)).unwrap_or(0);
        if position == 0 {
            info!("No saved position, so rebuilding from the start of the tape");
            return (0, CandleBook::default());
        }
        let candles = saved_candles(&self.sql_until_done(SAVED_CANDLES));
        info!("Resuming from position {} with {} candles", position, candles.len());
        (position, CandleBook::resume(candles))
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
        self.sender.flush(&mut self.rows).map_err(|e| e.to_string())?;

        if let Some(position) = position {
            self.position_row.clear();
            write_position(&mut self.position_row, position).map_err(|e| e.to_string())?;
            self.sender.flush(&mut self.position_row).map_err(|e| e.to_string())?;
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

        let trade = ExecutionReportMessage { exec_type: ExecTypeEnum::Trade, ..sample_report() };
        store.record(&trade);
        assert_eq!(store.queued_rows(), 3);
    }

    #[test]
    fn a_report_that_cannot_be_written_leaves_no_partial_row() {
        let mut store = store();
        store.record(&sample_report());
        store.record(&ExecutionReportMessage { transact_time: u64::MAX, ..sample_report() });
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
    fn finds_the_http_address() {
        assert_eq!(http_base("http::addr=localhost:9000;").as_deref(), Ok("http://localhost:9000"));
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
        assert_eq!(first_long(&json!({"dataset": [[4096]]})), Some(4096));
        assert_eq!(first_long(&json!({"dataset": []})), None);
    }

    #[test]
    fn reads_saved_candles() {
        let result = json!({"dataset": [
            ["ITEM", 300_000_000i64, 1, 4, 1, 2, 5, 2, 7],
            ["OTHER", 600_000_000i64, 9, 9, 9, 9, 1, 1, 8],
        ]});

        let candles = saved_candles(&result);

        assert_eq!(candles.len(), 2);
        assert_eq!(candles[0].symbol, *b"ITEM\0\0");
        assert_eq!(candles[0].start, 300);
        assert_eq!(candles[0].close, 2);
        assert_eq!(candles[0].last_match_id, 7);
        assert_eq!(candles[1].start, 600);
    }

    #[test]
    fn unreadable_candles_are_skipped() {
        let result = json!({"dataset": [["TOOLONGNAME", 0, 1, 1, 1, 1, 1, 1, 1], ["ITEM", null]]});
        assert!(saved_candles(&result).is_empty());
    }

    #[test]
    fn tables_are_applied_once_their_transactions_match() {
        let applied = json!({"dataset": [["orders", 5, 5], ["candles", 2, 2], ["unrelated", 1, 9]]});
        let pending = json!({"dataset": [["orders", 5, 6]]});

        assert!(wal_applied(&applied));
        assert!(!wal_applied(&pending));
        assert!(!wal_applied(&json!({"error": "nope"})));
    }
}

//! Reads order history from QuestDB, the query view of the tape. It can lag behind the stream, so
//! anything that needs an up-to-date answer goes through `repeat::await_recorder` first.
//!
//! The recorder must keep two promises:
//!
//! - `orders` has one row per execution report, including filled, cancelled and rejected orders,
//!   with `ts` as its timestamp, and prices and quantities as `LONG` mantissas.
//! - `recorder_position` is one row holding the stream position it has reached. It advances for
//!   every message on the stream, not only the ones it stores.
//!
//! A position can become visible before the `orders` rows written ahead of it, because QuestDB
//! applies each table's commits separately. `wal_tables()` tells when those rows have landed.

use std::time::Duration;

use async_trait::async_trait;

use sbe::side_enum::SideEnum;

use serde::Deserialize;
use serde_json::Value;

use uuid::Uuid;

const REQUEST_TIMEOUT: Duration = Duration::from_millis(500);

const ORDERS_COLUMNS: &str = "cl_ord_id, last(symbol) symbol, last(ord_status) ord_status, \
     last(side) side, last(leaves_qty) leaves_qty, last(cum_qty) cum_qty, last(price) price, \
     last(avg_px) avg_px, min(ts) created_at, max(ts) updated_at";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderKey {
    pub account: Uuid,
    pub cl_ord_id: Uuid,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OrdersCommits {
    pub applied: i64,
    pub made: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordedStatus {
    Open,
    Filled,
    Canceled,
    Rejected,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecordedOrder {
    pub id: Uuid,
    pub symbol: String,
    pub side: SideEnum,
    pub status: RecordedStatus,
    pub leaves_mantissa: i64,
    pub filled_mantissa: i64,
    pub price_mantissa: Option<i64>,
    pub average_price_mantissa: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

/// An order that's still working, with what a cancel needs to find it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenOrder {
    pub account: Uuid,
    pub id: Uuid,
}

/// A player's orders. `symbol` must already be normalized.
#[derive(Clone, Debug, Default)]
pub struct OrderQuery {
    pub account: Uuid,
    pub cl_ord_id: Option<Uuid>,
    pub symbol: Option<String>,
    pub open_only: bool,
    pub limit: usize,
}

#[async_trait]
pub trait OrderStore: Send + Sync {
    /// The furthest stream position the recorder has finished with.
    async fn recorder_position(&self) -> Result<i64, String>;

    /// How far `orders` commits have been applied, and how far they've been made.
    async fn orders_commits(&self) -> Result<OrdersCommits, String>;

    /// Whether this order got as far as the engine in the last `window`. A rejected order doesn't
    /// count, because it never executed and may be sent again.
    async fn order_reached_engine(&self, key: &OrderKey, window: Duration) -> Result<bool, String>;

    async fn orders(&self, query: &OrderQuery) -> Result<Vec<RecordedOrder>, String>;

    /// Every player's working orders in one market. `symbol` must already be normalized.
    async fn open_orders_in_market(&self, symbol: &str, limit: usize) -> Result<Vec<OpenOrder>, String>;
}

#[derive(Deserialize)]
struct ExecReply {
    #[serde(default)]
    columns: Vec<ExecColumn>,
    #[serde(default)]
    dataset: Vec<Vec<Value>>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct ExecColumn {
    name: String,
}

/// Tables are created on first write, so a missing one just means nothing's been written yet.
fn parse_reply(body: &str) -> Result<Option<ExecReply>, String> {
    let reply: ExecReply = serde_json::from_str(body).map_err(|err| err.to_string())?;
    match &reply.error {
        Some(error) if error.contains("does not exist") => Ok(None),
        Some(error) => Err(error.clone()),
        None => Ok(Some(reply)),
    }
}

const POSITION_SQL: &str = "select position from recorder_position";
const COMMITS_SQL: &str = "select writerTxn, sequencerTxn from wal_tables() where name = 'orders'";

fn repeat_sql(key: &OrderKey, window: Duration) -> String {
    format!(
        "select count() from orders where account = '{}' and cl_ord_id = '{}' \
         and ord_status != 'rejected' and ts >= dateadd('s', -{}, now())",
        key.account,
        key.cl_ord_id,
        window.as_secs()
    )
}

/// A rejected repeat shares its original's client order ID, but it says nothing about that order.
/// Left in, it would read as the latest state of an order that's still working, and a game could
/// release live escrow. Rows with no reason are null, and they have to stay.
const NOT_A_REPEAT: &str = "(ord_rej_reason is null or ord_rej_reason != 'duplicate_order')";

/// Values reach this SQL as parsed UUIDs and normalized symbols, so none can contain a quote.
fn orders_sql(query: &OrderQuery) -> String {
    let mut filters = format!("account = '{}' and {NOT_A_REPEAT}", query.account);
    if let Some(id) = query.cl_ord_id {
        filters.push_str(&format!(" and cl_ord_id = '{id}'"));
    }
    if let Some(symbol) = &query.symbol {
        filters.push_str(&format!(" and symbol = '{symbol}'"));
    }
    let open = if query.open_only {
        " where ord_status in ('new', 'partially_filled')"
    } else {
        ""
    };
    format!(
        "select * from (select {ORDERS_COLUMNS} from orders where {filters}){open} \
         order by created_at desc limit {}",
        query.limit
    )
}

/// Two players can share a client order ID, so rows group by account too. They also group by the
/// engine's order ID, because a rejected repeat of a working order shares its client order ID. The
/// repeat's reject would otherwise read as the working order's latest state and hide it.
fn open_orders_sql(symbol: &str, limit: usize) -> String {
    format!(
        "select account, cl_ord_id from (select account, cl_ord_id, order_id, \
         last(ord_status) ord_status from orders where symbol = '{symbol}') \
         where ord_status in ('new', 'partially_filled') limit {limit}"
    )
}

fn parse_open_orders(reply: &ExecReply) -> Result<Vec<OpenOrder>, String> {
    let column = |name: &str| {
        reply
            .columns
            .iter()
            .position(|column| column.name == name)
            .ok_or_else(|| format!("no column {name}"))
    };
    let (account, id) = (column("account")?, column("cl_ord_id")?);
    let uuid = |row: &[Value], at: usize| -> Result<Uuid, String> {
        let text = row.get(at).and_then(Value::as_str).ok_or("expected a UUID")?;
        Uuid::parse_str(text).map_err(|err| err.to_string())
    };
    reply
        .dataset
        .iter()
        .map(|row| Ok(OpenOrder { account: uuid(row, account)?, id: uuid(row, id)? }))
        .collect()
}

fn parse_status(status: &str) -> Result<RecordedStatus, String> {
    match status {
        "new" | "partially_filled" => Ok(RecordedStatus::Open),
        "filled" => Ok(RecordedStatus::Filled),
        "canceled" => Ok(RecordedStatus::Canceled),
        "rejected" => Ok(RecordedStatus::Rejected),
        other => Err(format!("unknown ord_status {other:?}")),
    }
}

fn parse_orders(reply: &ExecReply) -> Result<Vec<RecordedOrder>, String> {
    let column = |name: &str| {
        reply
            .columns
            .iter()
            .position(|column| column.name == name)
            .ok_or_else(|| format!("no column {name}"))
    };
    let (id, symbol, status) = (column("cl_ord_id")?, column("symbol")?, column("ord_status")?);
    let (side, leaves, filled) = (column("side")?, column("leaves_qty")?, column("cum_qty")?);
    let (price, average) = (column("price")?, column("avg_px")?);
    let (created_at, updated_at) = (column("created_at")?, column("updated_at")?);

    let text = |row: &[Value], at: usize| -> Result<String, String> {
        row[at].as_str().map(str::to_string).ok_or_else(|| format!("expected text, got {}", row[at]))
    };
    let number = |row: &[Value], at: usize| row[at].as_i64();

    reply
        .dataset
        .iter()
        .map(|row| {
            if row.len() != reply.columns.len() {
                return Err("row is the wrong length".to_string());
            }
            Ok(RecordedOrder {
                id: Uuid::parse_str(&text(row, id)?).map_err(|err| err.to_string())?,
                symbol: text(row, symbol)?,
                side: match text(row, side)?.as_str() {
                    "buy" => SideEnum::Buy,
                    "sell" => SideEnum::Sell,
                    other => return Err(format!("unknown side {other}")),
                },
                status: parse_status(&text(row, status)?)?,
                leaves_mantissa: number(row, leaves).ok_or("leaves_qty is not a number")?,
                filled_mantissa: number(row, filled).ok_or("cum_qty is not a number")?,
                price_mantissa: number(row, price),
                average_price_mantissa: number(row, average),
                created_at: text(row, created_at)?,
                updated_at: text(row, updated_at)?,
            })
        })
        .collect()
}

fn first_number(reply: &ExecReply) -> Option<i64> {
    reply.dataset.first()?.first()?.as_i64()
}

fn parse_commits(reply: &ExecReply) -> Result<OrdersCommits, String> {
    // No row means the table doesn't exist yet, so there's nothing to wait for.
    let Some(row) = reply.dataset.first() else {
        return Ok(OrdersCommits::default());
    };
    match row.as_slice() {
        [applied, made] => match (applied.as_i64(), made.as_i64()) {
            (Some(applied), Some(made)) => Ok(OrdersCommits { applied, made }),
            _ => Err(format!("unexpected commits {row:?}")),
        },
        _ => Err(format!("unexpected commits {row:?}")),
    }
}

pub struct QuestDb {
    url: String,
    client: reqwest::Client,
}

impl QuestDb {
    pub fn new(url: &str) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|err| err.to_string())?;
        Ok(Self { url: format!("{}/exec", url.trim_end_matches('/')), client })
    }

    async fn exec(&self, sql: &str) -> Result<Option<ExecReply>, String> {
        let response = self
            .client
            .get(&self.url)
            .query(&[("query", sql)])
            .send()
            .await
            .map_err(|err| err.to_string())?;
        // QuestDB reports a bad query with a 400 and an error body.
        let status = response.status();
        let body = response.text().await.map_err(|err| err.to_string())?;
        if status.is_server_error() {
            return Err(format!("{status}: {body}"));
        }
        parse_reply(&body)
    }
}

#[async_trait]
impl OrderStore for QuestDb {
    async fn recorder_position(&self) -> Result<i64, String> {
        let reply = self.exec(POSITION_SQL).await?;
        Ok(reply.as_ref().and_then(first_number).unwrap_or(0))
    }

    async fn orders_commits(&self) -> Result<OrdersCommits, String> {
        match self.exec(COMMITS_SQL).await? {
            Some(reply) => parse_commits(&reply),
            None => Ok(OrdersCommits::default()),
        }
    }

    async fn order_reached_engine(&self, key: &OrderKey, window: Duration) -> Result<bool, String> {
        let reply = self.exec(&repeat_sql(key, window)).await?;
        Ok(reply.as_ref().and_then(first_number).unwrap_or(0) > 0)
    }

    async fn orders(&self, query: &OrderQuery) -> Result<Vec<RecordedOrder>, String> {
        match self.exec(&orders_sql(query)).await? {
            Some(reply) => parse_orders(&reply),
            None => Ok(Vec::new()),
        }
    }

    async fn open_orders_in_market(&self, symbol: &str, limit: usize) -> Result<Vec<OpenOrder>, String> {
        match self.exec(&open_orders_sql(symbol, limit)).await? {
            Some(reply) => parse_open_orders(&reply),
            None => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> Uuid {
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap()
    }

    #[test]
    fn a_missing_table_is_not_an_error() {
        let body = r#"{"query":"x","error":"table does not exist [table=orders]","position":14}"#;
        assert!(parse_reply(body).unwrap().is_none());
    }

    #[test]
    fn a_query_error_is_an_error() {
        let body = r#"{"query":"x","error":"unexpected token [frm]","position":7}"#;
        assert_eq!(parse_reply(body).err().unwrap(), "unexpected token [frm]");
    }

    #[test]
    fn reads_a_position() {
        let body = r#"{"columns":[{"name":"max","type":"LONG"}],"dataset":[[4096]],"count":1}"#;
        assert_eq!(first_number(&parse_reply(body).unwrap().unwrap()), Some(4096));
        // An empty table gives a null.
        let body = r#"{"columns":[{"name":"max","type":"LONG"}],"dataset":[[null]],"count":1}"#;
        assert_eq!(first_number(&parse_reply(body).unwrap().unwrap()), None);
    }

    #[test]
    fn reads_orders() {
        let body = r#"{
            "columns": [
                {"name":"cl_ord_id","type":"VARCHAR"},{"name":"symbol","type":"SYMBOL"},
                {"name":"ord_status","type":"SYMBOL"},{"name":"side","type":"SYMBOL"},
                {"name":"leaves_qty","type":"LONG"},{"name":"cum_qty","type":"LONG"},
                {"name":"price","type":"LONG"},{"name":"avg_px","type":"LONG"},
                {"name":"created_at","type":"TIMESTAMP"},{"name":"updated_at","type":"TIMESTAMP"}
            ],
            "dataset": [
                ["22222222-2222-2222-2222-222222222222","ITEM","partially_filled","buy",
                 150000000,50000000,225000000,225000000,
                 "2026-11-04T12:00:00.000000Z","2026-11-04T12:00:01.000000Z"],
                ["33333333-3333-3333-3333-333333333333","ITEM","canceled","sell",
                 100000000,0,null,null,
                 "2026-11-04T12:00:02.000000Z","2026-11-04T12:00:02.000000Z"]
            ],
            "count": 2
        }"#;
        let orders = parse_orders(&parse_reply(body).unwrap().unwrap()).unwrap();
        assert_eq!(orders.len(), 2);
        assert_eq!(orders[0].status, RecordedStatus::Open);
        assert_eq!(orders[0].side, SideEnum::Buy);
        assert_eq!(orders[0].leaves_mantissa, 150_000_000);
        assert_eq!(orders[0].price_mantissa, Some(225_000_000));
        assert_eq!(orders[1].status, RecordedStatus::Canceled);
        assert_eq!(orders[1].side, SideEnum::Sell);
        assert_eq!(orders[1].price_mantissa, None);
    }

    #[test]
    fn reads_commit_progress() {
        let body = r#"{"columns":[{"name":"writerTxn"},{"name":"sequencerTxn"}],"dataset":[[7,9]]}"#;
        let reply = parse_reply(body).unwrap().unwrap();
        assert_eq!(parse_commits(&reply), Ok(OrdersCommits { applied: 7, made: 9 }));

        let body = r#"{"columns":[{"name":"writerTxn"},{"name":"sequencerTxn"}],"dataset":[]}"#;
        let reply = parse_reply(body).unwrap().unwrap();
        assert_eq!(parse_commits(&reply), Ok(OrdersCommits::default()));

        let body = r#"{"columns":[{"name":"writerTxn"}],"dataset":[[7]]}"#;
        assert!(parse_commits(&parse_reply(body).unwrap().unwrap()).is_err());
    }

    #[test]
    fn order_queries_leave_out_rejected_repeats() {
        let sql = orders_sql(&OrderQuery { account: account(), limit: 1, ..Default::default() });
        assert!(sql.contains(&format!("from orders where account = '{}' and (ord_rej_reason is null or ord_rej_reason != 'duplicate_order')", account())), "{sql}");
    }

    /// Run by hand against a scratch QuestDB, because it writes rows:
    /// `QUESTDB_SCRATCH_URL=http://localhost:19000 cargo test -p api real_questdb -- --ignored`
    #[tokio::test]
    #[ignore = "needs a scratch QuestDB"]
    async fn real_questdb_keeps_a_repeated_order_working() {
        const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS orders (
            ts TIMESTAMP, symbol SYMBOL, exec_type SYMBOL, ord_status SYMBOL, side SYMBOL,
            ord_rej_reason SYMBOL, account UUID, cl_ord_id UUID, order_id LONG, exec_id LONG,
            price LONG, ord_qty LONG, leaves_qty LONG, cum_qty LONG, avg_px LONG
        ) TIMESTAMP(ts) PARTITION BY DAY WAL DEDUP UPSERT KEYS(ts, exec_id)";

        let db = QuestDb::new(&std::env::var("QUESTDB_SCRATCH_URL").unwrap()).unwrap();
        db.exec(SCHEMA).await.unwrap();

        // Fresh IDs each run, because rows that share a time and exec ID replace each other.
        let (player, resting, partial) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let base = i64::from(Uuid::new_v4().as_u128() as u32) * 10;
        let symbol = format!("T{:05X}", base as u32 & 0xFFFFF);
        let rows = [
            (resting, "new", "new", "null", 1, 200, 0),
            (partial, "new", "new", "null", 2, 200, 0),
            (partial, "trade", "partially_filled", "null", 2, 150, 50),
            (resting, "rejected", "rejected", "'duplicate_order'", 3, 200, 0),
            (partial, "rejected", "rejected", "'duplicate_order'", 4, 200, 0),
        ];
        for (at, (id, exec_type, status, reason, order_id, leaves, filled)) in rows.iter().enumerate() {
            let sql = format!(
                "insert into orders values ('2026-11-04T12:00:{at:02}.000000Z', '{symbol}', '{exec_type}', \
                 '{status}', 'buy', {reason}, '{player}', '{id}', {order_id}, {}, 100, 200, {leaves}, \
                 {filled}, null)",
                base + at as i64,
            );
            db.exec(&sql).await.unwrap();
        }

        // WAL rows land a moment after the insert.
        let count = format!("select count() from orders where account = '{player}'");
        for _ in 0..100 {
            let reply = db.exec(&count).await.unwrap().unwrap();
            if first_number(&reply) == Some(rows.len() as i64) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let query = OrderQuery { account: player, limit: 10, ..Default::default() };
        let listed = db.orders(&query).await.unwrap();
        assert_eq!(listed.len(), 2, "{listed:?}");
        assert!(listed.iter().all(|order| order.status == RecordedStatus::Open), "{listed:?}");

        let open = OrderQuery { open_only: true, ..query.clone() };
        assert_eq!(db.orders(&open).await.unwrap().len(), 2, "cancel-all must find both");

        let one = OrderQuery { cl_ord_id: Some(partial), limit: 1, ..query };
        let order = db.orders(&one).await.unwrap().remove(0);
        assert_eq!(order.status, RecordedStatus::Open);
        assert_eq!((order.leaves_mantissa, order.filled_mantissa), (150, 50));
        assert!(order.created_at.contains("12:00:01"), "{}", order.created_at);

        let market = db.open_orders_in_market(&symbol, 10).await.unwrap();
        assert_eq!(market.len(), 2, "{market:?}");
    }

    #[test]
    fn a_market_query_spans_players_and_keeps_each_order_apart() {
        let sql = open_orders_sql("ITEM", 25);
        assert!(sql.contains("symbol = 'ITEM'"));
        assert!(!sql.contains("account ="), "every player: {sql}");
        assert!(sql.contains("select account, cl_ord_id, order_id,"), "{sql}");
        assert!(sql.contains("ord_status in ('new', 'partially_filled')"));
        assert!(sql.ends_with("limit 25"));
    }

    #[test]
    fn reads_open_orders() {
        let body = r#"{
            "columns": [{"name":"account","type":"UUID"},{"name":"cl_ord_id","type":"UUID"}],
            "dataset": [["11111111-1111-1111-1111-111111111111","22222222-2222-2222-2222-222222222222"]]
        }"#;
        let orders = parse_open_orders(&parse_reply(body).unwrap().unwrap()).unwrap();
        assert_eq!(orders[0].account, account());
        assert_eq!(orders[0].id, Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap());

        let body = r#"{"columns":[{"name":"account"},{"name":"cl_ord_id"}],"dataset":[[null,"x"]]}"#;
        assert!(parse_open_orders(&parse_reply(body).unwrap().unwrap()).is_err());
    }

    #[test]
    fn an_unknown_status_is_refused() {
        assert!(parse_status("mystery").is_err());
    }

    #[test]
    fn repeat_checks_skip_rejected_orders_and_stay_in_the_window() {
        let key = OrderKey { account: account(), cl_ord_id: Uuid::nil() };
        let sql = repeat_sql(&key, Duration::from_secs(300));
        assert!(sql.contains("ord_status != 'rejected'"));
        assert!(sql.contains("ts >= dateadd('s', -300, now())"));
        assert!(sql.contains(&format!("account = '{}'", account())));
    }

    #[test]
    fn order_queries_apply_every_filter() {
        let id = Uuid::nil();
        let sql = orders_sql(&OrderQuery {
            account: account(),
            cl_ord_id: Some(id),
            symbol: Some("ITEM".to_string()),
            open_only: true,
            limit: 25,
        });
        assert!(sql.contains(&format!("cl_ord_id = '{id}'")));
        assert!(sql.contains("symbol = 'ITEM'"));
        assert!(sql.contains("ord_status in ('new', 'partially_filled')"));
        assert!(sql.ends_with("limit 25"));

        let sql = orders_sql(&OrderQuery { account: account(), limit: 1, ..Default::default() });
        assert!(!sql.contains("ord_status in"));
        assert!(!sql.contains("symbol ="));
    }
}

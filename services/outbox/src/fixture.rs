//! Builds tape frames and a fake game for the tests.

use crate::frame::{Id, TERM_MARKER_TEMPLATE};

use sbe::WriteBuf;
use sbe::cxl_rej_reason_enum::CxlRejReasonEnum;
use sbe::cxl_rej_response_to_enum::CxlRejResponseToEnum;
use sbe::exec_type_enum::ExecTypeEnum;
use sbe::execution_report_codec::{ExecutionReportEncoder, SBE_BLOCK_LENGTH};
use sbe::message_header_codec::ENCODED_LENGTH;
use sbe::ord_rej_reason_enum::OrdRejReasonEnum;
use sbe::ord_status_enum::OrdStatusEnum;
use sbe::order_cancel_reject_codec::OrderCancelRejectEncoder;
use sbe::side_enum::SideEnum;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub const SLOT: usize = ENCODED_LENGTH + SBE_BLOCK_LENGTH as usize;
/// Frames are padded to 32 bytes, and a 138 byte slot takes 192 with its Aeron header.
pub const FRAME_STRIDE: i64 = 192;
pub const QTY: i64 = 100_000_000;

#[derive(Clone, Copy)]
pub struct Report {
    pub exec_id: u64,
    pub order_id: u64,
    pub exec_type: ExecTypeEnum,
    pub ord_status: OrdStatusEnum,
    pub reason: OrdRejReasonEnum,
    pub side: SideEnum,
    pub account: Id,
    pub cl_ord_id: Id,
    pub match_id: u64,
    pub price: i64,
    pub ord_qty: i64,
    pub leaves_qty: i64,
    pub cum_qty: i64,
    pub last_qty: i64,
    pub last_px: i64,
}

impl Default for Report {
    fn default() -> Self {
        Self {
            exec_id: 1,
            order_id: 1,
            exec_type: ExecTypeEnum::New,
            ord_status: OrdStatusEnum::New,
            reason: OrdRejReasonEnum::NullVal,
            side: SideEnum::Buy,
            account: [1, 1],
            cl_ord_id: [2, 2],
            match_id: u64::MAX,
            price: 100 * QTY,
            ord_qty: 10 * QTY,
            leaves_qty: 10 * QTY,
            cum_qty: 0,
            last_qty: i64::MIN,
            last_px: i64::MIN,
        }
    }
}

pub fn report(report: Report) -> Vec<u8> {
    let mut slot = vec![0u8; SLOT];
    let mut encoder = ExecutionReportEncoder::default().wrap(WriteBuf::new(&mut slot), ENCODED_LENGTH);
    let mut header = encoder.header(0);
    encoder = header.parent().expect("header");

    encoder.account(&report.account);
    encoder.cl_ord_id(&report.cl_ord_id);
    encoder.trd_match_id(report.match_id);
    encoder.order_id(report.order_id);
    encoder.exec_id(report.exec_id);
    encoder.symbol(b"SWORD\0");
    encoder.exec_type(report.exec_type);
    encoder.ord_status(report.ord_status);
    encoder.ord_rej_reason(report.reason);
    encoder.side(report.side);

    let mut time = encoder.transact_time_encoder();
    time.time(1_700_000_000_000_000_000 + report.exec_id);
    encoder = time.parent().expect("time");
    for (index, value) in [report.price, report.ord_qty, report.last_qty, report.last_px, report.leaves_qty, report.cum_qty, report.last_px].iter().enumerate() {
        let mut decimal = match index {
            0 => encoder.price_encoder(),
            1 => encoder.order_qty_encoder(),
            2 => encoder.last_qty_encoder(),
            3 => encoder.last_px_encoder(),
            4 => encoder.leaves_qty_encoder(),
            5 => encoder.cum_qty_encoder(),
            _ => encoder.avg_px_encoder(),
        };
        decimal.mantissa(*value);
        encoder = decimal.parent().expect("decimal");
    }
    slot
}

pub fn cancel_reject(cl_ord_id: Id, orig_cl_ord_id: Id) -> Vec<u8> {
    let mut slot = vec![0u8; SLOT];
    let mut encoder = OrderCancelRejectEncoder::default().wrap(WriteBuf::new(&mut slot), ENCODED_LENGTH);
    let mut header = encoder.header(0);
    encoder = header.parent().expect("header");
    encoder.cl_ord_id(&cl_ord_id);
    encoder.orig_cl_ord_id(&orig_cl_ord_id);
    encoder.order_id(u64::MAX);
    encoder.ord_status(OrdStatusEnum::NullVal);
    encoder.cxl_rej_response_to(CxlRejResponseToEnum::OrderCancelRequest);
    encoder.cxl_rej_reason(CxlRejReasonEnum::UnknownOrder);
    slot
}

pub fn term_marker(term: i64) -> Vec<u8> {
    let mut slot = vec![0u8; SLOT];
    slot[0..2].copy_from_slice(&8u16.to_le_bytes());
    slot[2..4].copy_from_slice(&TERM_MARKER_TEMPLATE.to_le_bytes());
    slot[8..16].copy_from_slice(&term.to_le_bytes());
    slot
}

/// A tape the way the engine writes it: a term marker, then for each trade two new orders, the
/// two fill legs, a cancel of what's left, a rejected order, a repeat of a working order and a
/// cancel reject.
pub fn tape(rounds: u64) -> Vec<(i64, Vec<u8>)> {
    let mut frames = vec![term_marker(0)];
    let mut exec_id = 0;
    let mut order_id = 0;
    let mut next_exec = || {
        exec_id += 1;
        exec_id
    };
    let mut next_order = || {
        order_id += 1;
        order_id
    };

    for round in 0..rounds {
        let buyer = Report { account: [1, round + 1], cl_ord_id: [10, round + 1], side: SideEnum::Buy, ..Report::default() };
        let seller = Report { account: [2, round + 1], cl_ord_id: [20, round + 1], side: SideEnum::Sell, ord_qty: 4 * QTY, leaves_qty: 4 * QTY, ..Report::default() };
        let buy_order = next_order();
        let sell_order = next_order();

        frames.push(report(Report { exec_id: next_exec(), order_id: buy_order, ..buyer }));
        frames.push(report(Report { exec_id: next_exec(), order_id: sell_order, ..seller }));

        let fill = Report { exec_type: ExecTypeEnum::Trade, match_id: round + 1, last_qty: 4 * QTY, last_px: 100 * QTY, ..Report::default() };
        frames.push(report(Report {
            exec_id: next_exec(),
            order_id: sell_order,
            ord_status: OrdStatusEnum::Filled,
            leaves_qty: 0,
            cum_qty: 4 * QTY,
            ord_qty: 4 * QTY,
            account: seller.account,
            cl_ord_id: seller.cl_ord_id,
            side: SideEnum::Sell,
            ..fill
        }));
        frames.push(report(Report {
            exec_id: next_exec(),
            order_id: buy_order,
            ord_status: OrdStatusEnum::PartiallyFilled,
            leaves_qty: 6 * QTY,
            cum_qty: 4 * QTY,
            ..Report { account: buyer.account, cl_ord_id: buyer.cl_ord_id, ..fill }
        }));

        // A repeat of the working buy order, on a full book. Its escrow must stay held.
        frames.push(report(Report {
            exec_id: next_exec(),
            order_id: next_order(),
            exec_type: ExecTypeEnum::Rejected,
            ord_status: OrdStatusEnum::Rejected,
            reason: OrdRejReasonEnum::Other,
            ..buyer
        }));

        frames.push(report(Report {
            exec_id: next_exec(),
            order_id: buy_order,
            exec_type: ExecTypeEnum::Canceled,
            ord_status: OrdStatusEnum::Canceled,
            leaves_qty: 6 * QTY,
            cum_qty: 4 * QTY,
            ..buyer
        }));

        frames.push(report(Report {
            exec_id: next_exec(),
            order_id: next_order(),
            exec_type: ExecTypeEnum::Rejected,
            ord_status: OrdStatusEnum::Rejected,
            reason: OrdRejReasonEnum::Other,
            cl_ord_id: [30, round + 1],
            ..Report::default()
        }));

        next_exec();
        frames.push(cancel_reject([40, round + 1], [41, round + 1]));
        if round % 7 == 3 {
            frames.push(term_marker(round as i64));
        }
    }

    frames.into_iter().enumerate().map(|(index, frame)| ((index as i64 + 1) * FRAME_STRIDE, frame)).collect()
}

/// One request as the fake game saw it.
#[derive(Debug, Clone)]
pub struct Request {
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    /// Whether the game applied the events, whatever it answered.
    pub accepted: bool,
}

#[derive(Default)]
struct GameState {
    requests: Mutex<Vec<Request>>,
    fail_next: AtomicUsize,
    /// Take the request in, then hang up without answering, as if the game crashed after
    /// committing or the answer was lost.
    lose_answer_next: AtomicUsize,
    redirect_next: AtomicUsize,
}

/// A webhook endpoint on an ephemeral port.
#[derive(Clone)]
pub struct FakeGame {
    state: Arc<GameState>,
    pub url: String,
}

fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Request> {
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    if line.is_empty() {
        return None;
    }
    let mut headers = HashMap::new();
    loop {
        line.clear();
        reader.read_line(&mut line).ok()?;
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        let (name, value) = trimmed.split_once(':')?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    let length: usize = headers.get("content-length")?.parse().ok()?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Request { headers, body, accepted: false })
}

fn take(counter: &AtomicUsize) -> bool {
    let mut value = counter.load(Ordering::SeqCst);
    while value > 0 {
        match counter.compare_exchange(value, value - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(actual) => value = actual,
        }
    }
    false
}

fn serve_connection(stream: TcpStream, state: &GameState) {
    let mut writer = stream.try_clone().expect("clone the stream");
    let mut reader = BufReader::new(stream);
    while let Some(mut request) = read_request(&mut reader) {
        if take(&state.lose_answer_next) {
            request.accepted = true;
            state.requests.lock().expect("requests").push(request);
            return;
        }
        let answer: &[u8] = if take(&state.fail_next) {
            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n"
        } else if take(&state.redirect_next) {
            b"HTTP/1.1 301 Moved Permanently\r\nLocation: /elsewhere\r\nContent-Length: 0\r\n\r\n"
        } else {
            request.accepted = true;
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"
        };
        state.requests.lock().expect("requests").push(request);
        if writer.write_all(answer).is_err() {
            return;
        }
    }
}

impl FakeGame {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let url = format!("http://{}/webhook", listener.local_addr().expect("local address"));
        let state = Arc::new(GameState::default());
        let accepting = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = Arc::clone(&accepting);
                std::thread::spawn(move || serve_connection(stream, &state));
            }
        });
        Self { state, url }
    }

    pub fn fail_next(&self, count: usize) {
        self.state.fail_next.store(count, Ordering::SeqCst);
    }

    pub fn lose_answer_next(&self, count: usize) {
        self.state.lose_answer_next.store(count, Ordering::SeqCst);
    }

    pub fn redirect_next(&self, count: usize) {
        self.state.redirect_next.store(count, Ordering::SeqCst);
    }

    pub fn requests(&self) -> Vec<Request> {
        self.state.requests.lock().expect("requests").clone()
    }

    /// Every event the game accepted, in arrival order, repeats included.
    pub fn events(&self) -> Vec<serde_json::Value> {
        self.requests()
            .iter()
            .filter(|request| request.accepted)
            .flat_map(|request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).expect("a JSON body");
                body["events"].as_array().expect("an events array").clone()
            })
            .collect()
    }
}

/// A fresh directory that's removed with the test's value.
pub struct TestDir(pub PathBuf);

impl TestDir {
    pub fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("outbox-{name}-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the test directory");
        Self(dir)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

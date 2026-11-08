//! Engine tests that need no Aeron. Inbound messages are real SBE, and reports are read back out of
//! the publisher's ring, so these cover the same bytes a node would put on the tape.

use std::collections::BTreeMap;

use sbe::ReadBuf;
use sbe::WriteBuf;
use sbe::exec_type_enum::ExecTypeEnum;
use sbe::execution_report_codec::ExecutionReportDecoder;
use sbe::message_header_codec::{ENCODED_LENGTH, MessageHeaderDecoder};
use sbe::new_order_single_codec::{self, NewOrderSingleEncoder};
use sbe::ord_rej_reason_enum::OrdRejReasonEnum;
use sbe::ord_type_enum::OrdTypeEnum;
use sbe::order_cancel_request_codec::{self, OrderCancelRequestEncoder};
use sbe::side_enum::SideEnum;

use crate::dispatch;
use crate::handler::Handler;
use crate::publisher::{
    self, FRAME_LENGTH, OutputState, Publisher, RECOVERY_MARKER_TEMPLATE, RecoveryEvent,
    TERM_LENGTH, TERM_MARKER_TEMPLATE, frames_between, position_after,
};
use crate::recovery::TapeFold;
use crate::snapshot::{self, Loader};
use crate::types::{self, ORDER_RECORD, Order};

const CAPACITY: usize = 4_096;
const SYMBOLS: [[u8; 6]; 2] = [*b"ITEM01", *b"ITEM02"];

fn engine() -> Handler {
    Handler::with_capacity(Publisher::new(None, true), CAPACITY)
}

fn uuid(id: u128) -> [u64; 2] {
    [(id >> 64) as u64, id as u64]
}

#[derive(Clone, Copy)]
struct NewOrder {
    id: u128,
    account: u128,
    symbol: [u8; 6],
    side: SideEnum,
    price: Option<i64>,
    quantity: i64,
}

fn limit(id: u128, account: u128, side: SideEnum, price: i64, quantity: i64) -> NewOrder {
    NewOrder {
        id,
        account,
        symbol: SYMBOLS[0],
        side,
        price: Some(price),
        quantity,
    }
}

fn market(id: u128, account: u128, side: SideEnum, quantity: i64) -> NewOrder {
    NewOrder {
        id,
        account,
        symbol: SYMBOLS[0],
        side,
        price: None,
        quantity,
    }
}

#[derive(Clone, Copy)]
enum Inbound {
    New(NewOrder),
    Cancel {
        id: u128,
        target: u128,
        account: u128,
    },
}

fn encode_new(order: &NewOrder) -> Vec<u8> {
    let mut buffer = vec![0u8; ENCODED_LENGTH + new_order_single_codec::SBE_BLOCK_LENGTH as usize];
    let encoder = NewOrderSingleEncoder::default().wrap(WriteBuf::new(&mut buffer), ENCODED_LENGTH);
    let mut encoder = encoder.header(0).parent().expect("a header");
    encoder.cl_ord_id(&uuid(order.id));
    encoder.account(&uuid(order.account));
    encoder.symbol(&order.symbol);
    encoder.side(order.side);
    encoder.ord_type(if order.price.is_some() {
        OrdTypeEnum::Limit
    } else {
        OrdTypeEnum::Market
    });
    let mut quantity = encoder.order_qty_encoder();
    quantity.mantissa(order.quantity);
    let encoder = quantity.parent().expect("a parent");
    let mut price = encoder.price_encoder();
    price.mantissa(order.price.unwrap_or(i64::MIN));
    price.parent().expect("a parent");
    buffer
}

fn encode_cancel(id: u128, target: u128, account: u128) -> Vec<u8> {
    let mut buffer =
        vec![0u8; ENCODED_LENGTH + order_cancel_request_codec::SBE_BLOCK_LENGTH as usize];
    let encoder =
        OrderCancelRequestEncoder::default().wrap(WriteBuf::new(&mut buffer), ENCODED_LENGTH);
    let mut encoder = encoder.header(0).parent().expect("a header");
    encoder.cl_ord_id(&uuid(id));
    encoder.orig_cl_ord_id(&uuid(target));
    encoder.account(&uuid(account));
    buffer
}

/// Sends one inbound message at time `time`, the way a node does.
fn send(handler: &mut Handler, inbound: &Inbound, time: u64) {
    handler.publisher.set_cluster_time(time);
    match inbound {
        Inbound::New(order) => dispatch(&encode_new(order), handler),
        Inbound::Cancel {
            id,
            target,
            account,
        } => dispatch(&encode_cancel(*id, *target, *account), handler),
    };
}

fn run(handler: &mut Handler, inbound: &[Inbound]) {
    for (time, message) in inbound.iter().enumerate() {
        send(handler, message, time as u64 + 1);
    }
}

fn written(handler: &Handler) -> u64 {
    handler.publisher.state().written
}

/// The reports written since `from`, as a node would have put them on the tape.
fn tape(handler: &Handler, from: u64) -> Vec<Vec<u8>> {
    (from..written(handler))
        .map(|index| handler.publisher.report(index).to_vec())
        .collect()
}

fn with_report<T>(message: &[u8], read: impl FnOnce(ExecutionReportDecoder<'_>) -> T) -> T {
    let header: MessageHeaderDecoder<ReadBuf<'_>> =
        MessageHeaderDecoder::default().wrap(ReadBuf::new(message), 0);
    read(ExecutionReportDecoder::default().header(header, 0))
}

fn exec_type(message: &[u8]) -> ExecTypeEnum {
    with_report(message, |report| report.exec_type())
}

fn template(message: &[u8]) -> u16 {
    u16::from_le_bytes([message[2], message[3]])
}

/// One queue's orders: order ID, leaves, cumulative quantity, notional, account and client ID.
type Queue = Vec<(u64, i64, i64, i128, u128, u128)>;

/// The book as anything outside the engine could observe it: per queue, in time priority.
fn queues(handler: &Handler) -> BTreeMap<([u8; 6], u8, i64), Queue> {
    let mut levels: BTreeMap<_, Vec<_>> = BTreeMap::new();
    let mut orders: Vec<&Order> = handler.book.resting().collect();
    orders.sort_unstable_by_key(|order| order.sequence_number);
    for order in orders {
        levels
            .entry((order.symbol, order.side as u8, order.price))
            .or_default()
            .push((
                order.sequence_number,
                order.leaves_quantity,
                order.cumulative_quantity,
                order.total_notional,
                order.account,
                order.client_order_id,
            ));
    }
    levels
}

/// Takes real queue links, rather than sequence numbers, to prove the book would match in order.
fn head_of_queue(handler: &mut Handler, side: SideEnum, symbol: &[u8; 6]) -> Option<u64> {
    let best = if side == SideEnum::Buy {
        handler.book.best_bid(symbol)
    } else {
        handler.book.best_ask(symbol)
    };
    best.map(|order| order.sequence_number)
}

/// A fixed mix of everything that can happen: partial fills across levels, market orders that sweep
/// and run out, self-trades, duplicates, cancels that hit and miss, and two symbols.
fn scenario(seed: u64, messages: usize) -> Vec<Inbound> {
    let mut state = seed | 1;
    let mut next = move |limit: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % limit
    };
    let mut inbound = Vec::with_capacity(messages);
    let mut sent: Vec<(u128, u128)> = Vec::new();
    for index in 0..messages {
        let id = index as u128 + 1;
        let account = u128::from(next(5) + 1);
        let side = if next(2) == 0 {
            SideEnum::Buy
        } else {
            SideEnum::Sell
        };
        let symbol = SYMBOLS[next(2) as usize];
        let quantity = next(40) as i64 + 1;
        let message = match next(10) {
            0 | 1 if !sent.is_empty() => {
                let (target, owner) = sent[next(sent.len() as u64) as usize];
                Inbound::Cancel {
                    id,
                    target,
                    account: owner,
                }
            }
            2 if !sent.is_empty() => Inbound::Cancel {
                id,
                target: id + 1_000_000,
                account,
            },
            3 if !sent.is_empty() => {
                let (duplicate, owner) = sent[next(sent.len() as u64) as usize];
                Inbound::New(NewOrder {
                    id: duplicate,
                    account: owner,
                    symbol,
                    side,
                    price: Some(100),
                    quantity,
                })
            }
            4 => Inbound::New(NewOrder {
                id,
                account,
                symbol,
                side,
                price: None,
                quantity,
            }),
            _ => {
                let price = 95 + next(11) as i64;
                sent.push((id, account));
                Inbound::New(NewOrder {
                    id,
                    account,
                    symbol,
                    side,
                    price: Some(price),
                    quantity,
                })
            }
        };
        inbound.push(message);
    }
    inbound
}

/// Runs a full engine, recording where each inbound message's reports ended.
fn recorded(inbound: &[Inbound]) -> (Handler, Vec<u64>) {
    let mut handler = engine();
    let mut ends = Vec::with_capacity(inbound.len());
    for (time, message) in inbound.iter().enumerate() {
        send(&mut handler, message, time as u64 + 1);
        ends.push(written(&handler));
    }
    (handler, ends)
}

/// Rebuilds a fresh engine from the first `cut` reports of the tape, then finishes the message the
/// tape stopped inside.
fn rebuild_from(tape: &[Vec<u8>], cut: usize) -> Handler {
    let mut rebuilt = engine();
    let mut fold = TapeFold::seed(&rebuilt);
    for message in &tape[..cut] {
        fold.apply(message).expect("a valid tape");
    }
    fold.install(&mut rebuilt);
    fold.settle(&mut rebuilt).expect("a valid tape");
    rebuilt
}

#[test]
fn limit_orders_trade_at_the_resting_price_in_time_priority() {
    let mut handler = engine();
    run(
        &mut handler,
        &[
            Inbound::New(limit(1, 1, SideEnum::Sell, 100, 5)),
            Inbound::New(limit(2, 2, SideEnum::Sell, 100, 5)),
            Inbound::New(limit(3, 3, SideEnum::Buy, 101, 7)),
        ],
    );

    let reports = tape(&handler, 0);
    let last_px =
        |message: &Vec<u8>| with_report(message, |report| report.last_px_decoder().mantissa());
    assert_eq!(exec_type(&reports[3]), ExecTypeEnum::Trade);
    assert_eq!(last_px(&reports[3]), 100);
    assert_eq!(with_report(&reports[4], |report| report.order_id()), 1);
    assert_eq!(with_report(&reports[6], |report| report.order_id()), 2);
    assert_eq!(handler.book.pool.len(), 1);
    assert_eq!(
        handler
            .book
            .resting()
            .next()
            .map(|order| order.leaves_quantity),
        Some(3)
    );
}

#[test]
fn a_market_order_cancels_what_it_cannot_fill() {
    let mut handler = engine();
    run(
        &mut handler,
        &[
            Inbound::New(limit(1, 1, SideEnum::Sell, 100, 5)),
            Inbound::New(market(2, 2, SideEnum::Buy, 8)),
        ],
    );

    let reports = tape(&handler, 0);
    let last = reports.last().expect("reports");
    assert_eq!(exec_type(last), ExecTypeEnum::Canceled);
    assert_eq!(
        with_report(last, |report| report.leaves_qty_decoder().mantissa()),
        3
    );
    assert!(handler.book.pool.is_empty());
}

#[test]
fn a_self_trade_cancels_the_aggressor_and_leaves_the_resting_order() {
    let mut handler = engine();
    run(
        &mut handler,
        &[
            Inbound::New(limit(1, 1, SideEnum::Sell, 100, 5)),
            Inbound::New(limit(2, 1, SideEnum::Buy, 100, 5)),
        ],
    );

    assert_eq!(
        exec_type(tape(&handler, 0).last().expect("reports")),
        ExecTypeEnum::Canceled
    );
    assert_eq!(handler.book.pool.len(), 1);
    assert_eq!(handler.counter_match_id, 0);
}

#[test]
fn a_duplicate_client_order_id_is_rejected_and_spends_an_order_id() {
    let mut handler = engine();
    run(
        &mut handler,
        &[
            Inbound::New(limit(1, 1, SideEnum::Sell, 100, 5)),
            Inbound::New(limit(1, 1, SideEnum::Sell, 101, 5)),
        ],
    );

    let reports = tape(&handler, 0);
    assert_eq!(exec_type(&reports[1]), ExecTypeEnum::Rejected);
    assert_eq!(
        with_report(&reports[1], |report| report.ord_rej_reason()),
        OrdRejReasonEnum::DuplicateOrder
    );
    assert_eq!(handler.counter_order_id, 2);
}

#[test]
fn an_unknown_cancel_spends_an_exec_id_but_no_order_id() {
    let mut handler = engine();
    run(
        &mut handler,
        &[Inbound::Cancel {
            id: 1,
            target: 9,
            account: 1,
        }],
    );
    assert_eq!((handler.counter_exec_id, handler.counter_order_id), (1, 0));
}

#[test]
fn a_full_book_rejects_and_keeps_running() {
    let mut handler = Handler::with_capacity(Publisher::new(None, true), 1);
    let capacity = handler.book.pool.capacity();
    for id in 0..capacity as u128 {
        run(
            &mut handler,
            &[Inbound::New(limit(id + 1, 1, SideEnum::Sell, 100, 1))],
        );
    }
    run(
        &mut handler,
        &[Inbound::New(limit(
            capacity as u128 + 1,
            1,
            SideEnum::Sell,
            100,
            1,
        ))],
    );
    assert_eq!(
        exec_type(tape(&handler, 0).last().expect("reports")),
        ExecTypeEnum::Rejected
    );
    assert_eq!(handler.book.pool.len(), capacity);
}

#[test]
fn a_full_book_still_reports_a_repeat_of_a_resting_order_as_a_duplicate() {
    let mut handler = Handler::with_capacity(Publisher::new(None, true), 1);
    let capacity = handler.book.pool.capacity();
    for id in 0..capacity as u128 {
        run(
            &mut handler,
            &[Inbound::New(limit(id + 1, 1, SideEnum::Sell, 100, 1))],
        );
    }
    run(
        &mut handler,
        &[Inbound::New(limit(1, 1, SideEnum::Sell, 100, 1))],
    );

    let reports = tape(&handler, 0);
    let last = reports.last().expect("reports");
    assert_eq!(exec_type(last), ExecTypeEnum::Rejected);
    assert_eq!(
        with_report(last, |report| report.ord_rej_reason()),
        OrdRejReasonEnum::DuplicateOrder
    );
    assert_eq!(handler.book.pool.len(), capacity);
}

#[test]
fn matching_ignores_the_wall_clock() {
    let inbound = scenario(7, 500);
    let (first, _) = recorded(&inbound);
    let (second, _) = recorded(&inbound);
    assert_eq!(tape(&first, 0), tape(&second, 0));
}

#[test]
fn every_report_carries_the_cluster_time() {
    let mut handler = engine();
    send(
        &mut handler,
        &Inbound::New(limit(1, 1, SideEnum::Sell, 100, 5)),
        1_234,
    );
    let report = tape(&handler, 0).remove(0);
    assert_eq!(
        with_report(&report, |report| report.transact_time_decoder().time()),
        1_234
    );
}

#[test]
fn an_order_record_survives_encoding() {
    let mut order = Order::empty();
    order.price = -5;
    order.quantity = 10;
    order.leaves_quantity = 4;
    order.cumulative_quantity = 6;
    order.total_notional = -(1 << 70);
    order.sequence_number = 9;
    order.client_order_id = u128::MAX - 1;
    order.account = 1 << 90;
    order.transact_time = 77;
    order.symbol = *b"ITEM01";
    order.side = SideEnum::Sell;
    order.r#type = OrdTypeEnum::Limit;

    let mut record = [0u8; ORDER_RECORD];
    types::encode_order(&order, &mut record);
    let decoded = types::decode_order(&record);
    assert_eq!(decoded.total_notional, order.total_notional);
    assert_eq!(decoded.key(), order.key());
    assert_eq!(decoded.symbol, order.symbol);
    assert_eq!(decoded.side, order.side);
    assert_eq!(decoded.r#type, order.r#type);
    assert_eq!(
        (
            decoded.price,
            decoded.quantity,
            decoded.leaves_quantity,
            decoded.cumulative_quantity
        ),
        (
            order.price,
            order.quantity,
            order.leaves_quantity,
            order.cumulative_quantity
        )
    );
    assert_eq!(
        (decoded.sequence_number, decoded.transact_time),
        (order.sequence_number, order.transact_time)
    );
}

fn restored(handler: &Handler) -> Handler {
    let mut chunks = Vec::new();
    snapshot::write(handler, &handler.publisher.state(), &mut |chunk| {
        chunks.push(chunk.to_vec())
    });

    let mut restored = engine();
    let mut loader = Loader::default();
    for chunk in &chunks {
        assert!(
            chunk.len() <= 60_000,
            "a chunk must fit in one Aeron message"
        );
        loader.on_chunk(&mut restored, chunk);
    }
    loader.finish(&mut restored);
    restored
}

#[test]
fn a_snapshot_restores_the_book_counters_and_output_state() {
    let inbound = scenario(11, 2_000);
    let (handler, _) = recorded(&inbound);
    let restored = restored(&handler);

    assert_eq!(queues(&restored), queues(&handler));
    assert_eq!(restored.counter_order_id, handler.counter_order_id);
    assert_eq!(restored.counter_exec_id, handler.counter_exec_id);
    assert_eq!(restored.counter_match_id, handler.counter_match_id);
    assert_eq!(restored.publisher.state(), handler.publisher.state());
}

#[test]
fn a_snapshot_restores_time_priority_from_a_pool_with_holes() {
    let mut handler = engine();
    run(
        &mut handler,
        &[
            Inbound::New(limit(1, 1, SideEnum::Sell, 100, 1)),
            Inbound::New(limit(2, 2, SideEnum::Sell, 100, 1)),
            Inbound::New(limit(3, 3, SideEnum::Sell, 100, 1)),
            Inbound::Cancel {
                id: 4,
                target: 1,
                account: 1,
            },
            // This takes the slot order 1 freed, so the pool now lists it before order 2.
            Inbound::New(limit(5, 4, SideEnum::Sell, 100, 1)),
        ],
    );

    let mut restored = restored(&handler);
    assert_eq!(
        head_of_queue(&mut restored, SideEnum::Sell, &SYMBOLS[0]),
        Some(2)
    );
}

#[test]
fn a_restored_engine_writes_the_same_bytes_as_one_that_never_stopped() {
    let inbound = scenario(13, 3_000);
    let (split, rest) = inbound.split_at(1_500);
    let (mut running, _) = recorded(split);
    let mut restarted = restored(&running);

    let (from_running, from_restarted) = (written(&running), written(&restarted));
    run(&mut running, rest);
    run(&mut restarted, rest);
    assert_eq!(
        tape(&restarted, from_restarted),
        tape(&running, from_running)
    );
}

#[test]
fn a_rebuild_from_any_position_finishes_as_the_lost_engine_would_have() {
    let inbound = scenario(17, 400);
    let (original, ends) = recorded(&inbound);
    let reports = tape(&original, 0);
    assert!(
        reports
            .iter()
            .any(|message| exec_type(message) == ExecTypeEnum::Trade),
        "the scenario must trade"
    );

    for cut in 0..=reports.len() {
        let mut rebuilt = rebuild_from(&reports, cut);
        let settled = tape(&rebuilt, 0);

        // Whatever the tape stopped inside finishes first, and matches what was lost.
        assert_eq!(
            settled,
            reports[cut..cut + settled.len()],
            "settling after report {cut}"
        );

        // Then every later message writes exactly the original bytes.
        let resume = ends
            .iter()
            .position(|end| *end as usize == cut + settled.len());
        let next = resume.map_or(0, |index| index + 1);
        let position = written(&rebuilt);
        for (index, message) in inbound.iter().enumerate().skip(next) {
            send(&mut rebuilt, message, index as u64 + 1);
        }
        assert_eq!(
            tape(&rebuilt, position),
            reports[cut + settled.len()..],
            "continuing after report {cut}"
        );
        assert_eq!(
            queues(&rebuilt),
            queues(&original),
            "final book after report {cut}"
        );
    }
}

#[test]
fn a_rebuild_keeps_open_orders_and_their_queue_positions() {
    let inbound = scenario(19, 2_000);
    let (original, _) = recorded(&inbound);
    let reports = tape(&original, 0);
    let mut rebuilt = rebuild_from(&reports, reports.len());
    let mut original = original;

    assert!(
        !queues(&original).is_empty(),
        "the scenario must leave open orders"
    );
    assert_eq!(queues(&rebuilt), queues(&original));
    assert_eq!(rebuilt.counter_order_id, original.counter_order_id);
    assert_eq!(rebuilt.counter_exec_id, original.counter_exec_id);
    assert_eq!(rebuilt.counter_match_id, original.counter_match_id);
    for symbol in &SYMBOLS {
        for side in [SideEnum::Buy, SideEnum::Sell] {
            assert_eq!(
                head_of_queue(&mut rebuilt, side, symbol),
                head_of_queue(&mut original, side, symbol)
            );
        }
    }
}

#[test]
fn a_rebuild_on_top_of_a_snapshot_matches_one_from_the_start() {
    let inbound = scenario(23, 2_000);
    let (split, rest) = inbound.split_at(1_000);
    let (snapshotted, _) = recorded(split);
    let mut base = restored(&snapshotted);

    let mut original = restored(&snapshotted);
    let from = written(&original);
    run(&mut original, rest);

    let mut fold = TapeFold::seed(&base);
    for message in tape(&original, from) {
        fold.apply(&message).expect("a valid tape");
    }
    fold.install(&mut base);
    fold.settle(&mut base).expect("a valid tape");

    assert_eq!(queues(&base), queues(&original));
    assert_eq!(base.counter_exec_id, original.counter_exec_id);
}

#[test]
fn a_rebuild_skips_markers_and_counts_cancel_rejects() {
    let mut original = engine();
    run(
        &mut original,
        &[Inbound::New(limit(1, 1, SideEnum::Sell, 100, 5))],
    );
    original.publisher.publish_term_marker(1);
    run(
        &mut original,
        &[Inbound::Cancel {
            id: 2,
            target: 9,
            account: 1,
        }],
    );
    original
        .publisher
        .publish_recovery_marker(RecoveryEvent::Rebuilt, 0, 1);

    let reports = tape(&original, 0);
    assert_eq!(template(&reports[1]), TERM_MARKER_TEMPLATE);
    assert_eq!(template(&reports[3]), RECOVERY_MARKER_TEMPLATE);

    let rebuilt = rebuild_from(&reports, reports.len());
    assert_eq!(rebuilt.counter_exec_id, original.counter_exec_id);
    assert_eq!(queues(&rebuilt), queues(&original));
}

#[test]
fn a_tape_with_a_gap_is_refused() {
    let inbound = [
        Inbound::New(limit(1, 1, SideEnum::Sell, 100, 5)),
        Inbound::New(limit(2, 2, SideEnum::Buy, 100, 5)),
        Inbound::New(limit(3, 2, SideEnum::Buy, 90, 5)),
    ];
    let (original, _) = recorded(&inbound);
    let mut reports = tape(&original, 0);
    reports.remove(3); // The resting side of the trade.

    let handler = engine();
    let mut fold = TapeFold::seed(&handler);
    let outcome: Result<Vec<()>, String> =
        reports.iter().map(|message| fold.apply(message)).collect();
    assert!(outcome.is_err());
}

#[test]
fn cancel_all_cancels_oldest_first_whatever_the_pool_layout() {
    let inbound = scenario(29, 2_000);
    let (mut original, _) = recorded(&inbound);
    let mut restarted = restored(&original);

    let (from_original, from_restarted) = (written(&original), written(&restarted));
    let open = original.book.pool.len();
    assert!(open > 1, "the scenario must leave open orders");
    original.publisher.set_cluster_time(99);
    restarted.publisher.set_cluster_time(99);
    assert_eq!(original.cancel_all(), open);
    assert_eq!(restarted.cancel_all(), open);

    let reports = tape(&original, from_original);
    assert_eq!(reports, tape(&restarted, from_restarted));
    assert_eq!(reports.len(), open);
    assert!(
        reports
            .iter()
            .all(|message| exec_type(message) == ExecTypeEnum::Canceled)
    );
    let ids: Vec<u64> = reports
        .iter()
        .map(|message| with_report(message, |report| report.order_id()))
        .collect();
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(original.book.pool.is_empty());
}

#[test]
fn the_engine_trades_normally_after_cancel_all() {
    let mut handler = engine();
    run(
        &mut handler,
        &[Inbound::New(limit(1, 1, SideEnum::Sell, 100, 5))],
    );
    handler.cancel_all();
    run(
        &mut handler,
        &[
            Inbound::New(limit(2, 1, SideEnum::Sell, 100, 5)),
            Inbound::New(limit(3, 2, SideEnum::Buy, 100, 5)),
        ],
    );
    assert_eq!(handler.counter_match_id, 1);
    assert!(handler.book.pool.is_empty());
}

/// Aeron's appender, frame by frame: a frame that doesn't fit pads out the term.
fn append(position: i64) -> i64 {
    let offset = position % TERM_LENGTH;
    let start = if offset + FRAME_LENGTH > TERM_LENGTH {
        position - offset + TERM_LENGTH
    } else {
        position
    };
    start + FRAME_LENGTH
}

#[test]
fn the_frame_length_is_a_report_aligned_to_32_bytes() {
    assert_eq!(FRAME_LENGTH, 192);
}

#[test]
fn stream_positions_match_aeron_across_term_boundaries() {
    let frames_per_term = (TERM_LENGTH / FRAME_LENGTH) as u64;
    let start = TERM_LENGTH - 5 * FRAME_LENGTH - 64;
    let mut position = start;
    for frames in 1..=(2 * frames_per_term + 20) {
        position = append(position);
        if frames < 12 || frames % 1_000 == 0 || frames + 20 > 2 * frames_per_term {
            assert_eq!(
                position_after(start, frames),
                position,
                "after {frames} frames"
            );
            assert_eq!(
                frames_between(start, position),
                frames,
                "between, after {frames} frames"
            );
        }
    }
}

#[test]
fn the_publisher_tracks_its_position_from_a_restored_base() {
    let mut publisher = Publisher::new(None, true);
    let position = 3 * TERM_LENGTH - FRAME_LENGTH;
    publisher.restore(OutputState {
        written: 40,
        digest: 0,
        position,
        quiet_position: 0,
        last_marker_written: u64::MAX,
    });
    assert_eq!(publisher.output_position(), position);
    publisher.publish_term_marker(1);
    publisher.publish_term_marker(2);
    assert_eq!(publisher.output_position(), append(append(position)));
}

#[test]
fn term_markers_leave_a_quiet_range_that_a_report_ends() {
    let mut handler = engine();
    run(
        &mut handler,
        &[Inbound::New(limit(1, 1, SideEnum::Sell, 100, 5))],
    );
    let after_report = handler.publisher.output_position();
    assert_eq!(
        handler.publisher.quiet_range(),
        (after_report, after_report)
    );

    handler.publisher.publish_term_marker(1);
    handler.publisher.publish_term_marker(2);
    assert_eq!(
        handler.publisher.quiet_range(),
        (after_report, after_report + 2 * FRAME_LENGTH)
    );

    run(
        &mut handler,
        &[Inbound::Cancel {
            id: 2,
            target: 9,
            account: 1,
        }],
    );
    let current = handler.publisher.output_position();
    assert_eq!(handler.publisher.quiet_range(), (current, current));
}

#[test]
fn a_snapshot_carries_the_quiet_range() {
    let mut handler = engine();
    run(
        &mut handler,
        &[Inbound::New(limit(1, 1, SideEnum::Sell, 100, 5))],
    );
    handler.publisher.publish_term_marker(1);
    assert_eq!(
        restored(&handler).publisher.quiet_range(),
        handler.publisher.quiet_range()
    );
}

#[test]
fn a_rebase_moves_the_stream_to_the_tape() {
    let mut publisher = Publisher::new(None, true);
    publisher.publish_term_marker(1);
    publisher.rebase(19_200);
    assert_eq!(publisher.quiet_range(), (19_200, 19_200));
    publisher.publish_term_marker(2);
    assert_eq!(publisher.output_position(), 19_392);
}

#[test]
fn a_held_engine_keeps_more_reports_than_it_can_send_without_blocking() {
    let mut publisher = Publisher::new(None, true);
    for term in 0..(publisher::RING_LENGTH as i64 + 5) {
        publisher.publish_term_marker(term);
    }
    assert_eq!(publisher.state().written, publisher::RING_LENGTH as u64 + 5);
}

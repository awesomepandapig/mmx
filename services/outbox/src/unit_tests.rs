use crate::fixture::{self, QTY, Report, SLOT, TestDir};
use crate::frame::{self, ExecType, Frame, OrdStatus, RejectReason};
use crate::journal::{JOURNAL_FILE, Journal, JournalReader, Record};
use crate::ledger::{Ledger, REPEAT_OF_OPEN_ORDER};
use crate::webhook::{self, EventType};

use sbe::exec_type_enum::ExecTypeEnum;
use sbe::ord_rej_reason_enum::OrdRejReasonEnum;
use sbe::ord_status_enum::OrdStatusEnum;

use std::fs::OpenOptions;
use std::io::Write;

fn record(position: i64, exec_id: u64) -> Record {
    Record {
        position,
        exec_id,
        flags: 0,
        frame: vec![position as u8; SLOT],
    }
}

fn write_records(dir: &std::path::Path, count: i64) {
    let mut journal = Journal::open(dir, |_| Ok(())).expect("open");
    for index in 1..=count {
        journal.append(&record(index * 192, index as u64));
    }
    journal.sync().expect("sync");
}

fn reopen(dir: &std::path::Path) -> (Journal, Vec<Record>) {
    let mut seen = Vec::new();
    let journal = Journal::open(dir, |record| {
        seen.push(record.clone());
        Ok(())
    })
    .expect("reopen");
    (journal, seen)
}

#[test]
fn decodes_reports_markers_and_cancel_rejects() {
    let Frame::Execution(report) = frame::decode(&fixture::report(Report {
        exec_id: 7,
        order_id: 3,
        exec_type: ExecTypeEnum::Rejected,
        ord_status: OrdStatusEnum::Rejected,
        reason: OrdRejReasonEnum::DuplicateOrder,
        ..Report::default()
    })) else {
        panic!("expected a report");
    };
    assert_eq!((report.exec_id, report.order_id), (7, 3));
    assert_eq!(report.exec_type, ExecType::Rejected);
    assert_eq!(report.ord_status, OrdStatus::Rejected);
    assert_eq!(report.reject_reason, Some(RejectReason::DuplicateOrder));
    assert_eq!(report.symbol, "SWORD");
    assert_eq!(report.match_id, None);
    assert_eq!(report.last_qty, None);

    assert_eq!(frame::decode(&fixture::term_marker(4)), Frame::TermMarker);
    assert_eq!(
        frame::decode(&fixture::cancel_reject([1, 2], [3, 4])),
        Frame::CancelReject {
            cl_ord_id: [1, 2],
            orig_cl_ord_id: [3, 4]
        }
    );
}

#[test]
fn short_or_foreign_frames_are_unknown_rather_than_read_past_the_end() {
    assert_eq!(frame::decode(&[0; 4]), Frame::Unknown(0));
    let mut short = fixture::report(Report::default());
    short.truncate(20);
    assert_eq!(frame::decode(&short), Frame::Unknown(3));
    let mut foreign = fixture::term_marker(0);
    foreign[2] = 77;
    assert_eq!(frame::decode(&foreign), Frame::Unknown(77));
}

#[test]
fn ledger_refuses_a_gap_and_stays_put() {
    let mut ledger = Ledger::default();
    let first = frame::decode(&fixture::report(Report::default()));
    assert_eq!(ledger.apply(&first).expect("first").exec_id, 1);

    let skipped = frame::decode(&fixture::report(Report {
        exec_id: 3,
        ..Report::default()
    }));
    assert!(ledger.apply(&skipped).is_err());
    assert_eq!(ledger.last_exec_id(), 1);
}

#[test]
fn cancel_rejects_spend_an_exec_id_and_markers_dont() {
    let mut ledger = Ledger::default();
    assert_eq!(
        ledger
            .apply(&frame::decode(&fixture::term_marker(0)))
            .expect("marker")
            .exec_id,
        0
    );
    assert_eq!(
        ledger
            .apply(&frame::decode(&fixture::cancel_reject([1, 1], [2, 2])))
            .expect("reject")
            .exec_id,
        1
    );
    assert_eq!(
        ledger
            .apply(&frame::decode(&fixture::report(Report {
                exec_id: 2,
                ..Report::default()
            })))
            .expect("report")
            .exec_id,
        2
    );
}

fn rejected(exec_id: u64, order_id: u64, reason: OrdRejReasonEnum) -> Frame {
    frame::decode(&fixture::report(Report {
        exec_id,
        order_id,
        exec_type: ExecTypeEnum::Rejected,
        ord_status: OrdStatusEnum::Rejected,
        reason,
        ..Report::default()
    }))
}

#[test]
fn a_repeat_of_a_working_order_is_flagged_for_either_reason() {
    for reason in [OrdRejReasonEnum::DuplicateOrder, OrdRejReasonEnum::Other] {
        let mut ledger = Ledger::default();
        ledger
            .apply(&frame::decode(&fixture::report(Report::default())))
            .expect("new");
        assert_eq!(
            ledger.apply(&rejected(2, 2, reason)).expect("reject").flags,
            REPEAT_OF_OPEN_ORDER
        );
        assert_eq!(
            ledger.open_orders(),
            1,
            "the original order is still working"
        );
    }
}

#[test]
fn an_order_rejected_after_its_own_new_report_closes() {
    let mut ledger = Ledger::default();
    ledger
        .apply(&frame::decode(&fixture::report(Report::default())))
        .expect("new");
    assert_eq!(
        ledger
            .apply(&rejected(2, 1, OrdRejReasonEnum::Other))
            .expect("reject")
            .flags,
        0
    );
    assert_eq!(ledger.open_orders(), 0);
}

#[test]
fn a_client_order_id_can_be_reused_once_the_order_is_finished() {
    let mut ledger = Ledger::default();
    ledger
        .apply(&frame::decode(&fixture::report(Report::default())))
        .expect("new");
    let canceled = Report {
        exec_id: 2,
        exec_type: ExecTypeEnum::Canceled,
        ord_status: OrdStatusEnum::Canceled,
        ..Report::default()
    };
    ledger
        .apply(&frame::decode(&fixture::report(canceled)))
        .expect("cancel");
    assert_eq!(ledger.open_orders(), 0);
    assert_eq!(
        ledger
            .apply(&rejected(3, 2, OrdRejReasonEnum::Other))
            .expect("reject")
            .flags,
        0
    );
}

#[test]
fn journal_cuts_a_torn_tail_and_keeps_appending() {
    let dir = TestDir::new("torn");
    write_records(&dir.0, 3);

    // Half a record, as a crash in the middle of a write leaves it.
    let path = dir.0.join(JOURNAL_FILE);
    let mut torn = Vec::new();
    record(4 * 192, 4).encode_into(&mut torn);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("append")
        .write_all(&torn[..torn.len() / 2])
        .expect("tear");

    let (mut journal, seen) = reopen(&dir.0);
    assert_eq!(seen.len(), 3);
    assert_eq!(journal.position(), 3 * 192);
    assert_eq!(journal.previous_position(), 2 * 192);

    journal.append(&record(5 * 192, 5));
    journal.sync().expect("sync");
    drop(journal);
    assert_eq!(reopen(&dir.0).1.last().expect("a record").position, 5 * 192);
}

#[test]
fn journal_cuts_a_record_with_a_bad_checksum() {
    let dir = TestDir::new("crc");
    write_records(&dir.0, 3);

    let path = dir.0.join(JOURNAL_FILE);
    let mut bytes = std::fs::read(&path).expect("read");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&path, bytes).expect("write");

    let (journal, seen) = reopen(&dir.0);
    assert_eq!(seen.len(), 2);
    assert_eq!(journal.position(), 2 * 192);
}

#[test]
fn journal_keeps_records_in_memory_until_sync() {
    let dir = TestDir::new("pending");
    let mut journal = Journal::open(&dir.0, |_| Ok(())).expect("open");
    journal.append(&record(192, 1));
    journal.append(&record(384, 2));

    assert_eq!(
        std::fs::metadata(dir.0.join(JOURNAL_FILE))
            .expect("stat")
            .len(),
        0
    );
    assert_eq!(journal.synced_length(), 0);

    journal.sync().expect("sync");
    assert_eq!(
        std::fs::metadata(dir.0.join(JOURNAL_FILE))
            .expect("stat")
            .len(),
        journal.synced_length()
    );
}

#[test]
fn journal_is_owned_by_one_process() {
    let dir = TestDir::new("lock");
    let first = Journal::open(&dir.0, |_| Ok(())).expect("open");
    assert!(Journal::open(&dir.0, |_| Ok(())).is_err());
    drop(first);
    assert!(Journal::open(&dir.0, |_| Ok(())).is_ok());
}

#[test]
fn reader_stops_at_the_limit_and_reports_damage_below_it() {
    let dir = TestDir::new("reader");
    write_records(&dir.0, 3);
    let (journal, _) = reopen(&dir.0);
    let length = journal.synced_length();
    let one_record = length / 3;

    let mut reader = JournalReader::open(&dir.0, 0).expect("reader");
    assert_eq!(
        reader
            .next_below(one_record)
            .expect("read")
            .expect("a record")
            .position,
        192
    );
    assert_eq!(reader.next_below(one_record).expect("read"), None);
    assert_eq!(
        reader
            .next_below(length)
            .expect("read")
            .expect("a record")
            .position,
        384
    );

    let path = dir.0.join(JOURNAL_FILE);
    let mut bytes = std::fs::read(&path).expect("read");
    let middle = bytes.len() - 5;
    bytes[middle] ^= 0xff;
    std::fs::write(&path, bytes).expect("write");
    let mut reader = JournalReader::open(&dir.0, 2 * one_record).expect("reader");
    assert!(reader.next_below(length).is_err());
}

fn event_of(position: i64, frame: Vec<u8>, flags: u8, exec_id: u64) -> Option<webhook::Event> {
    webhook::event(&Record {
        position,
        exec_id,
        flags,
        frame,
    })
}

#[test]
fn fills_say_what_traded_and_only_the_last_is_terminal() {
    let fill = Report {
        exec_id: 3,
        exec_type: ExecTypeEnum::Trade,
        ord_status: OrdStatusEnum::PartiallyFilled,
        match_id: 9,
        last_qty: 2 * QTY + QTY / 2,
        last_px: 100 * QTY,
        cum_qty: 2 * QTY + QTY / 2,
        leaves_qty: 7 * QTY + QTY / 2,
        ..Report::default()
    };
    let partial = event_of(192, fixture::report(fill), 0, 3).expect("an event");
    assert_eq!(partial.event_type, EventType::Fill);
    assert!(!partial.terminal);
    assert_eq!(partial.release_qty, "0");
    assert_eq!(partial.last_qty.as_deref(), Some("2.5"));
    assert_eq!(partial.match_id, Some(9));

    let last = Report {
        ord_status: OrdStatusEnum::Filled,
        cum_qty: 10 * QTY,
        leaves_qty: 0,
        ..fill
    };
    let filled = event_of(384, fixture::report(last), 0, 4).expect("an event");
    assert!(filled.terminal);
    assert_eq!(filled.release_qty, "0");
}

#[test]
fn both_legs_of_a_trade_get_their_own_id() {
    let tape = fixture::tape(1);
    let legs: Vec<_> = tape
        .iter()
        .filter_map(|(position, frame)| event_of(*position, frame.clone(), 0, 0))
        .filter(|event| event.event_type == EventType::Fill)
        .collect();
    assert_eq!(legs.len(), 2);
    assert_eq!(legs[0].match_id, legs[1].match_id);
    assert_ne!(legs[0].fill_id, legs[1].fill_id);
}

#[test]
fn identical_cancel_rejects_keep_their_own_ids() {
    let reject = fixture::cancel_reject([1, 1], [2, 2]);
    assert_ne!(
        webhook::fill_id(192, &reject),
        webhook::fill_id(384, &reject)
    );
}

#[test]
fn a_reused_exec_id_with_different_content_gets_a_new_id() {
    let first = fixture::report(Report {
        last_qty: QTY,
        ..Report::default()
    });
    let second = fixture::report(Report {
        last_qty: 2 * QTY,
        ..Report::default()
    });
    assert_ne!(
        webhook::fill_id(192, &first),
        webhook::fill_id(192, &second)
    );
}

#[test]
fn a_cancel_releases_what_never_filled() {
    let cancel = Report {
        exec_type: ExecTypeEnum::Canceled,
        ord_status: OrdStatusEnum::Canceled,
        cum_qty: 4 * QTY,
        leaves_qty: 6 * QTY,
        ..Report::default()
    };
    let event = event_of(192, fixture::report(cancel), 0, 1).expect("an event");
    assert!(event.terminal);
    assert_eq!(event.release_qty, "6");
    assert_eq!(event.reason, None);
}

#[test]
fn a_reject_releases_everything_unless_it_repeats_a_working_order() {
    let reject = fixture::report(Report {
        exec_type: ExecTypeEnum::Rejected,
        ord_status: OrdStatusEnum::Rejected,
        reason: OrdRejReasonEnum::Other,
        ..Report::default()
    });
    let real = event_of(192, reject.clone(), 0, 1).expect("an event");
    assert!(real.terminal);
    assert_eq!(real.release_qty, "10");
    assert_eq!(real.reason, Some("other"));

    let repeat = event_of(192, reject, REPEAT_OF_OPEN_ORDER, 1).expect("an event");
    assert!(!repeat.terminal);
    assert_eq!(repeat.release_qty, "0");
}

#[test]
fn a_cancel_reject_never_touches_escrow() {
    let event = event_of(192, fixture::cancel_reject([1, 1], [2, 2]), 0, 1).expect("an event");
    assert_eq!(event.event_type, EventType::CancelRejected);
    assert!(!event.terminal);
    assert_eq!(event.release_qty, "0");
    assert_eq!(event.account, None);
    assert_eq!(
        event.orig_client_order_id.as_deref(),
        Some("00000000-0000-0002-0000-000000000002")
    );
}

#[test]
fn acknowledgements_and_markers_send_nothing() {
    assert_eq!(
        event_of(192, fixture::report(Report::default()), 0, 1),
        None
    );
    assert_eq!(event_of(192, fixture::term_marker(1), 0, 0), None);
}

#[test]
fn nulls_are_left_out_and_numbers_are_strings() {
    let market = Report {
        price: i64::MIN,
        exec_type: ExecTypeEnum::Canceled,
        ord_status: OrdStatusEnum::Canceled,
        ..Report::default()
    };
    let event = event_of(192, fixture::report(market), 0, 1).expect("an event");
    let json = serde_json::to_value(&event).expect("json");
    for absent in [
        "price",
        "match_id",
        "last_qty",
        "last_px",
        "avg_px",
        "orig_client_order_id",
        "reason",
    ] {
        assert!(json.get(absent).is_none(), "{absent} should be left out");
    }
    assert_eq!(json["type"], "canceled");
    assert_eq!(json["status"], "canceled");
    assert_eq!(json["side"], "buy");
    assert_eq!(json["order_qty"], "10");
    assert_eq!(
        json["client_order_id"],
        "00000000-0000-0002-0000-000000000002"
    );
    assert!(json["transact_time"].is_string());
}

#[test]
fn decimals_match_the_wire_scale() {
    assert_eq!(webhook::decimal(0), "0");
    assert_eq!(webhook::decimal(150_000_000), "1.5");
    assert_eq!(webhook::decimal(1), "0.00000001");
    assert_eq!(webhook::decimal(-250_000_000), "-2.5");
}

#[test]
fn hmac_matches_the_rfc_4231_vector() {
    assert_eq!(
        webhook::hmac_hex(b"Jefe", &[b"what do ya want ", b"for nothing?"]),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
}

#[test]
fn the_signature_covers_the_timestamp_and_the_body() {
    let signature = webhook::signature(b"secret", 1000, b"{}");
    assert_eq!(
        signature,
        format!("v1={}", webhook::hmac_hex(b"secret", &[b"1000.{}"]))
    );
    assert_ne!(signature, webhook::signature(b"secret", 1001, b"{}"));
    assert_ne!(signature, webhook::signature(b"secret", 1000, b"{ }"));
    assert_ne!(signature, webhook::signature(b"other", 1000, b"{}"));
}

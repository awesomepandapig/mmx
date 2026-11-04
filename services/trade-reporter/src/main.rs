use sbe::ord_status_enum::OrdStatusEnum;

use std::hint::spin_loop;
use std::time::Instant;

use sbe::exec_type_enum::ExecTypeEnum;
use sbe::side_enum::SideEnum;

use log::{error, info};

use sbe::ReadBuf;
use sbe::execution_report_codec::ExecutionReportDecoder;
use sbe::message_header_codec::MessageHeaderDecoder;

const DECIMAL_NULL_VAL: i64 = i64::MIN;

use uuid::Uuid;

use questdb::ingress::{Buffer, Sender, TimestampNanos};

fn u64s_to_uuid(uuid: [u64; 2]) -> Uuid {
    let mut bytes = [0u8; 16];
    bytes[0..8].copy_from_slice(&uuid[0].to_be_bytes());
    bytes[8..16].copy_from_slice(&uuid[1].to_be_bytes());
    Uuid::from_bytes(bytes)
}

fn aeron_error_handler(code: i32, message: &str) {
    error!("Aeron: Error {}: {}", code, message);
}

pub fn read_message(buffer: &[u8], qdb_buffer: &mut Buffer, qdb_sender: &mut Sender) {
    let read_buf = ReadBuf::new(buffer);
    let header_decoder: MessageHeaderDecoder<ReadBuf<'_>> =
        MessageHeaderDecoder::default().wrap(read_buf, 0);

    match header_decoder.template_id() {
        3 => {
            let _result = write_to_db(header_decoder, qdb_buffer, qdb_sender);
            // println!("RESULT: {:?}", result);
        }
        4 => {}
        _ => {}
    }
}

fn write_to_db(
    header_decoder: MessageHeaderDecoder<ReadBuf<'_>>,
    qdb_buffer: &mut Buffer,
    qdb_sender: &mut Sender,
) -> questdb::Result<()> {
    let report: ExecutionReportDecoder<'_> =
        ExecutionReportDecoder::default().header(header_decoder, 0);

    let exec_type = report.exec_type();
    if !(exec_type == ExecTypeEnum::New || exec_type == ExecTypeEnum::Trade) {
        return Ok(());
    }

    let account = u64s_to_uuid(report.account()).to_string();
    let cl_ord_id = u64s_to_uuid(report.cl_ord_id()).to_string();

    let price_m = report.price_decoder().mantissa();
    let avg_px_m = report.avg_px_decoder().mantissa();

    let price = if price_m != DECIMAL_NULL_VAL {
        Some(price_m as f64 / 1e8)
    } else {
        None
    };

    let avg_px = if avg_px_m != DECIMAL_NULL_VAL {
        Some(avg_px_m as f64 / 1e8)
    } else {
        None
    };

    let leaves_qty = report.leaves_qty_decoder().mantissa() as f64 / 1e8;
    let cum_qty = report.cum_qty_decoder().mantissa() as f64 / 1e8;

    let ord_status = match report.ord_status() {
        OrdStatusEnum::New => "new",
        OrdStatusEnum::PartiallyFilled => "partially_filled",
        OrdStatusEnum::Canceled => "canceled",
        _ => return Ok(()),
    };

    let side_bool = match report.side() {
        SideEnum::Buy => true,
        SideEnum::Sell => false,
        _ => return Ok(()),
    };

    let symbol_raw = report.symbol();
    let symbol = match std::str::from_utf8(&symbol_raw) {
        Ok(s) => s.trim_end_matches('\0'), // strip padding nulls
        Err(_) => return Ok(()),           // ignore invalid UTF-8
    };

    let timestamp = TimestampNanos::new(report.transact_time_decoder().time() as i64);

    let mut builder = qdb_buffer
        .table("orders")?
        .symbol("symbol", symbol)?
        .symbol("ord_status", ord_status)?
        .column_str("account", &account)?
        .column_str("cl_ord_id", &cl_ord_id)?
        .column_f64("leaves_qty", leaves_qty)?
        .column_f64("cum_qty", cum_qty)?
        .column_bool("side", side_bool)?;

    if let Some(p) = price {
        builder = builder.column_f64("price", p)?;
    }
    if let Some(a) = avg_px {
        builder = builder.column_f64("avg_px", a)?;
    }

    builder.at(timestamp)?;

    qdb_sender.flush(qdb_buffer)?;
    Ok(())
}

fn main() -> questdb::Result<()> {
    env_logger::init();

    let start_time = Instant::now();

    let (_aeron, subscription) = match transport::subscribe_to_executions(aeron_error_handler) {
        Ok(client) => client,
        Err(e) => {
            error!("Aeron: Failed to subscribe: {:?}", e);
            return Ok(());
        }
    };

    let startup_duration = start_time.elapsed();
    info!(
        "Startup complete in {:.2?} — ready to accept requests",
        startup_duration
    );

    let questdb_conf = transport::env_or("QUESTDB_CONF", "http::addr=localhost:9000;");
    let mut qdb_sender = Sender::from_conf(questdb_conf)?;
    let mut qdb_buffer = Buffer::new();

    let mut msg_count = 0;
    let flush_every = 1000;

    loop {
        let fragments_read = subscription
            .poll_fn(
                |buffer, _header| {
                    read_message(buffer, &mut qdb_buffer, &mut qdb_sender);
                    msg_count += 1;
                    if msg_count >= flush_every {
                        let _ = qdb_sender.flush(&mut qdb_buffer);
                        msg_count = 0;
                    }
                },
                1024,
            )
            .expect("Failed to poll Aeron subscription");

        if fragments_read == 0 {
            spin_loop();
        }
    }
}

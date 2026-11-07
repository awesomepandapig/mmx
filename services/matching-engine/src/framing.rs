//! Validate a whole cluster session batch before matching or running an admin command.
//!
//! The wire format has no variable-length fields. Accept only the schema versions and fixed
//! blocks understood here: accepting a short/extended block would desynchronise the next message.

use sbe::{SBE_SCHEMA_ID, SBE_SCHEMA_VERSION, message_header_codec::ENCODED_LENGTH};
use sbe::{new_order_single_codec, order_cancel_request_codec};

use crate::admin;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidBatch {
    pub offset: usize,
    pub reason: &'static str,
}

/// Calls `apply` only after every envelope in the batch has been checked. In particular an
/// invalid tail cannot leave a successfully matched prefix behind on any replaying node.
pub fn apply_batch(payload: &[u8], mut apply: impl FnMut(&[u8])) -> Result<(), InvalidBatch> {
    if payload.is_empty() {
        return Err(InvalidBatch {
            offset: 0,
            reason: "empty session batch",
        });
    }
    let mut offset = 0;
    while offset < payload.len() {
        let length =
            message_length(&payload[offset..]).map_err(|reason| InvalidBatch { offset, reason })?;
        offset += length;
    }

    let mut remaining = payload;
    while !remaining.is_empty() {
        // The immutable bytes and their complete lengths were verified above.
        let length = ENCODED_LENGTH + usize::from(u16::from_le_bytes([remaining[0], remaining[1]]));
        let (message, tail) = remaining.split_at(length);
        apply(message);
        remaining = tail;
    }
    Ok(())
}

fn message_length(message: &[u8]) -> Result<usize, &'static str> {
    if message.len() < ENCODED_LENGTH {
        return Err("truncated message header");
    }
    let field = |at| u16::from_le_bytes([message[at], message[at + 1]]);
    let (block, template, schema, version) = (field(0), field(2), field(4), field(6));
    let expected = if let Some(block) = admin::block_length(template) {
        // Admin commands predate the trading SBE schema and use a zero schema/version header.
        if schema != 0 || version != 0 {
            return Err("unsupported admin schema or version");
        }
        block
    } else {
        if schema != SBE_SCHEMA_ID || version != SBE_SCHEMA_VERSION {
            return Err("unsupported trading schema or version");
        }
        match template {
            new_order_single_codec::SBE_TEMPLATE_ID => new_order_single_codec::SBE_BLOCK_LENGTH,
            order_cancel_request_codec::SBE_TEMPLATE_ID => {
                order_cancel_request_codec::SBE_BLOCK_LENGTH
            }
            _ => return Err("unsupported session template"),
        }
    };
    if block != expected {
        return Err("unsupported message block length");
    }
    let length = ENCODED_LENGTH + usize::from(block);
    if message.len() < length {
        return Err("truncated message body");
    }
    Ok(length)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::AdminCommand;
    use crate::handler::Handler;
    use crate::publisher::Publisher;
    use sbe::WriteBuf;
    use sbe::new_order_single_codec::NewOrderSingleEncoder;
    use sbe::ord_type_enum::OrdTypeEnum;
    use sbe::side_enum::SideEnum;

    fn order() -> Vec<u8> {
        let mut bytes =
            vec![0; ENCODED_LENGTH + usize::from(new_order_single_codec::SBE_BLOCK_LENGTH)];
        let encoder =
            NewOrderSingleEncoder::default().wrap(WriteBuf::new(&mut bytes), ENCODED_LENGTH);
        let mut encoder = encoder.header(0).parent().expect("header parent");
        encoder.cl_ord_id(&[0, 1]);
        encoder.account(&[0, 2]);
        encoder.symbol(b"ITEM01");
        encoder.side(SideEnum::Buy);
        encoder.ord_type(OrdTypeEnum::Limit);
        let mut quantity = encoder.order_qty_encoder();
        quantity.mantissa(10);
        let mut price = quantity.parent().expect("quantity parent").price_encoder();
        price.mantissa(100);
        bytes
    }

    #[test]
    fn every_truncated_order_is_rejected_before_matching() {
        let bytes = order();
        for length in 0..bytes.len() {
            let mut handler = Handler::with_capacity(Publisher::new(None, true), 16);
            assert!(
                apply_batch(&bytes[..length], |message| {
                    crate::dispatch(message, &mut handler);
                })
                .is_err()
            );
            assert_eq!(handler.counter_order_id, 0, "length {length}");
            assert_eq!(handler.publisher.state().written, 0, "length {length}");
        }
    }

    #[test]
    fn valid_prefix_invalid_tail_has_no_engine_or_admin_effects() {
        let prefix = order();
        let tail = AdminCommand::Rebuild {
            expected: 0,
            position: 192,
        }
        .encode();
        for length in 1..tail.len() {
            let mut batch = prefix.clone();
            batch.extend_from_slice(&tail[..length]);
            let mut handler = Handler::with_capacity(Publisher::new(None, true), 16);
            let mut admin_calls = 0;
            assert!(
                apply_batch(&batch, |message| {
                    if admin::is_admin(message) {
                        admin_calls += 1;
                    } else {
                        crate::dispatch(message, &mut handler);
                    }
                })
                .is_err()
            );
            assert_eq!(handler.counter_order_id, 0);
            assert_eq!(handler.publisher.state().written, 0);
            assert_eq!(admin_calls, 0);
        }
    }

    #[test]
    fn supported_mixed_batch_dispatches_in_order() {
        let commands = [
            AdminCommand::CancelAll,
            AdminCommand::TapeSwept { position: 192 },
            AdminCommand::Rebuild {
                expected: 192,
                position: 384,
            },
        ];
        let mut batch = order();
        for command in commands {
            batch.extend(command.encode());
        }
        let mut handler = Handler::with_capacity(Publisher::new(None, true), 16);
        let mut observed = Vec::new();
        apply_batch(&batch, |message| {
            if admin::is_admin(message) {
                observed.push(AdminCommand::decode(message).expect("validated command"));
            } else {
                crate::dispatch(message, &mut handler);
            }
        })
        .expect("valid batch");
        assert_eq!(handler.counter_order_id, 1);
        assert_eq!(observed, commands);
    }

    #[test]
    fn schema_version_template_and_block_mismatches_reject_the_whole_batch() {
        let messages = [order(), AdminCommand::CancelAll.encode()];
        for original in messages {
            for (at, value) in [(0, 1u16), (2, 999), (4, 999), (6, 999)] {
                let mut invalid = original.clone();
                invalid[at..at + 2].copy_from_slice(&value.to_le_bytes());
                let mut batch = order();
                batch.extend(invalid);
                let mut calls = 0;
                assert!(apply_batch(&batch, |_| calls += 1).is_err(), "field {at}");
                assert_eq!(calls, 0);
            }
        }
    }

    #[test]
    fn cancel_block_and_every_truncated_admin_command_are_checked() {
        let mut cancel =
            vec![0; ENCODED_LENGTH + usize::from(order_cancel_request_codec::SBE_BLOCK_LENGTH)];
        cancel[0..2].copy_from_slice(&order_cancel_request_codec::SBE_BLOCK_LENGTH.to_le_bytes());
        cancel[2..4].copy_from_slice(&order_cancel_request_codec::SBE_TEMPLATE_ID.to_le_bytes());
        cancel[4..6].copy_from_slice(&SBE_SCHEMA_ID.to_le_bytes());
        cancel[6..8].copy_from_slice(&SBE_SCHEMA_VERSION.to_le_bytes());
        assert!(apply_batch(&cancel, |_| {}).is_ok());
        for message in [
            cancel,
            AdminCommand::CancelAll.encode(),
            AdminCommand::TapeSwept { position: 192 }.encode(),
            AdminCommand::Rebuild {
                expected: 0,
                position: 192,
            }
            .encode(),
        ] {
            for length in 0..message.len() {
                assert!(
                    apply_batch(&message[..length], |_| panic!(
                        "truncated message dispatched"
                    ))
                    .is_err()
                );
            }
        }
    }
}

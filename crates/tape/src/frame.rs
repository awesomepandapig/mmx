//! Classifies what arrives on the report stream.

use sbe::ReadBuf;
use sbe::execution_report_codec;
use sbe::message_header_codec::MessageHeaderDecoder;

use crate::messages::{ExecutionReportMessage, decode_execution_report};

const HEADER_LENGTH: usize = 8;
const REPORT_TEMPLATE: u16 = execution_report_codec::SBE_TEMPLATE_ID;
const CANCEL_REJECT_TEMPLATE: u16 = 4;
const TERM_MARKER_TEMPLATE: u16 = 100;

#[derive(Debug)]
pub enum Frame {
    Report(Box<ExecutionReportMessage>),
    /// Spends an exec ID, but changes no order and carries no exec ID to read.
    CancelReject,
    /// Written at the start of each leadership term, holding the term's ID.
    TermMarker(i64),
    Unknown(u16),
    /// Too short to hold what its header claims.
    Malformed,
}

pub fn parse(buffer: &[u8]) -> Frame {
    if buffer.len() < HEADER_LENGTH {
        return Frame::Malformed;
    }
    let template_id = u16::from_le_bytes([buffer[2], buffer[3]]);
    match template_id {
        REPORT_TEMPLATE => {
            if buffer.len() < HEADER_LENGTH + execution_report_codec::SBE_BLOCK_LENGTH as usize {
                return Frame::Malformed;
            }
            let header = MessageHeaderDecoder::default().wrap(ReadBuf::new(buffer), 0);
            Frame::Report(Box::new(decode_execution_report(header)))
        }
        CANCEL_REJECT_TEMPLATE => Frame::CancelReject,
        TERM_MARKER_TEMPLATE => match buffer.get(HEADER_LENGTH..HEADER_LENGTH + 8) {
            Some(term) => {
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(term);
                Frame::TermMarker(i64::from_le_bytes(bytes))
            }
            None => Frame::Malformed,
        },
        template_id => Frame::Unknown(template_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use sbe::WriteBuf;
    use sbe::exec_type_enum::ExecTypeEnum;
    use sbe::execution_report_codec::ExecutionReportEncoder;
    use sbe::message_header_codec::ENCODED_LENGTH;

    fn encoded_report(order_id: u64) -> Vec<u8> {
        let mut bytes = vec![0u8; 256];
        let encoder =
            ExecutionReportEncoder::default().wrap(WriteBuf::new(&mut bytes), ENCODED_LENGTH);
        let mut encoder = encoder.header(0).parent().expect("header fits");
        encoder.order_id(order_id);
        encoder.exec_type(ExecTypeEnum::Trade);
        bytes
    }

    fn header(template_id: u16) -> Vec<u8> {
        let mut bytes = vec![0u8; 8];
        bytes[0..2].copy_from_slice(&8u16.to_le_bytes());
        bytes[2..4].copy_from_slice(&template_id.to_le_bytes());
        bytes
    }

    #[test]
    fn decodes_execution_reports() {
        let Frame::Report(report) = parse(&encoded_report(42)) else {
            panic!("expected a report");
        };
        assert_eq!(report.order_id, 42);
        assert_eq!(report.exec_type, ExecTypeEnum::Trade);
    }

    #[test]
    fn reads_the_term_marker() {
        let mut bytes = header(100);
        bytes.extend_from_slice(&7i64.to_le_bytes());
        assert!(matches!(parse(&bytes), Frame::TermMarker(7)));
    }

    #[test]
    fn cancel_rejects_are_recognised() {
        assert!(matches!(parse(&header(4)), Frame::CancelReject));
    }

    #[test]
    fn unexpected_templates_are_returned_instead_of_panicking() {
        assert!(matches!(parse(&header(77)), Frame::Unknown(77)));
    }

    #[test]
    fn short_frames_are_malformed() {
        assert!(matches!(parse(&[1, 2, 3]), Frame::Malformed));
        assert!(matches!(parse(&header(3)), Frame::Malformed));
        assert!(matches!(parse(&header(100)), Frame::Malformed));
        assert!(matches!(parse(&encoded_report(1)[..40]), Frame::Malformed));
    }
}

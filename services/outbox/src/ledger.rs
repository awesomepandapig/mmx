//! What the outbox has to remember between reports.
//!
//! The ledger is rebuilt from the journal on startup, so it needs no storage of its own.

use crate::frame::{ExecType, Frame, Id, RejectReason};

use std::collections::HashMap;

/// A reject aimed at a client order ID that's still working. The reject is about the repeat, so
/// the original order's escrow has to stay held.
pub const REPEAT_OF_OPEN_ORDER: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub exec_id: u64,
    pub flags: u8,
}

#[derive(Default)]
pub struct Ledger {
    last_exec_id: u64,
    open: HashMap<(Id, Id), u64>,
}

impl Ledger {
    pub fn last_exec_id(&self) -> u64 {
        self.last_exec_id
    }

    pub fn open_orders(&self) -> usize {
        self.open.len()
    }

    /// Stamps a frame with its exec ID and flags, or refuses it if an exec ID went missing. A
    /// refused frame changes nothing.
    pub fn apply(&mut self, frame: &Frame) -> Result<Stamp, String> {
        let expected = self.last_exec_id + 1;
        match frame {
            Frame::Execution(report) => {
                if report.exec_id != expected {
                    return Err(format!("expected exec_id {expected}, got {}", report.exec_id));
                }
                self.last_exec_id = expected;

                let key = (report.account, report.cl_ord_id);
                let open = self.open.get(&key).copied();
                if report.exec_type == ExecType::New {
                    self.open.insert(key, report.order_id);
                    return Ok(Stamp { exec_id: expected, flags: 0 });
                }

                // A full book rejects before the engine checks for duplicates, so a repeat can
                // arrive with reason `Other`. Only the order ID tells it apart from a real reject.
                let repeat = report.exec_type == ExecType::Rejected
                    && (report.reject_reason == Some(RejectReason::DuplicateOrder) || open.is_some_and(|id| id != report.order_id));
                if repeat {
                    return Ok(Stamp { exec_id: expected, flags: REPEAT_OF_OPEN_ORDER });
                }

                if report.ord_status.is_terminal() && open == Some(report.order_id) {
                    self.open.remove(&key);
                }
                Ok(Stamp { exec_id: expected, flags: 0 })
            }
            Frame::CancelReject { .. } => {
                self.last_exec_id = expected;
                Ok(Stamp { exec_id: expected, flags: 0 })
            }
            Frame::TermMarker | Frame::Unknown(_) => Ok(Stamp { exec_id: self.last_exec_id, flags: 0 }),
        }
    }
}

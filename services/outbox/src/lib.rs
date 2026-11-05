//! The durable step between the tape and the game.
//!
//! Frames come off the tape, reach the journal and are flushed to disk, and only then can a
//! webhook carry them. See `DESIGN.md` for the guarantees and the replication plan.

pub mod delivery;
pub mod frame;
pub mod journal;
pub mod ledger;
pub mod outbox;
pub mod status;
pub mod webhook;

#[cfg(test)]
mod fixture;
#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
mod unit_tests;

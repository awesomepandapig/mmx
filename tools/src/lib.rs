//! Developer tooling for inspecting and verifying the exchange from outside.
//!
//! Everything here observes or drives the Aeron streams, so nothing in `matching-engine` depends on it.

pub mod args;
pub mod check;
pub mod event;
pub mod wire;

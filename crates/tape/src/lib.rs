//! What every consumer of the report stream shares: decoding, book and candle state, following
//! the tape, and saving a position to resume from.
//!
//! The tape is the record, so consumers can fall behind it without affecting the engine. Nothing
//! on the data path panics: a report that doesn't fit the local state is logged and skipped.

pub mod book;
pub mod candle;
pub mod checkpoint;
#[cfg(feature = "feed")]
pub mod feed;
pub mod follower;
pub mod format;
pub mod frame;
pub mod messages;
pub mod source;

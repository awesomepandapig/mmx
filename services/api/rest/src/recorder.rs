//! How far this gateway has read the report stream, and how to wait for the recorder to match it.
//! Anything that must see the latest state in QuestDB waits here first.

use super::errors::AppError;
use super::store::OrderStore;

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use log::warn;

use tokio::time::{Instant as TokioInstant, sleep};

const UNKNOWN_POSITION: i64 = -1;
const CATCH_UP_DEADLINE: Duration = Duration::from_secs(2);
const CATCH_UP_POLL: Duration = Duration::from_millis(20);

/// What the thread reading the report stream shares with request handlers.
pub struct ReportStream {
    position: AtomicI64,
}

impl Default for ReportStream {
    fn default() -> Self {
        Self {
            position: AtomicI64::new(UNKNOWN_POSITION),
        }
    }
}

impl ReportStream {
    pub fn position(&self) -> i64 {
        self.position.load(Ordering::Acquire)
    }

    pub fn advance_to(&self, position: i64) {
        self.position.fetch_max(position, Ordering::AcqRel);
    }
}

/// Waits for the recorder to pass the position the stream has reached now, so that anything
/// published before this call can be read back.
pub async fn await_recorder(stream: &ReportStream, store: &dyn OrderStore) -> Result<(), AppError> {
    let target = stream.position();
    if target == UNKNOWN_POSITION {
        return Err(AppError::RecorderBehind);
    }
    if target == 0 {
        return Ok(()); // Nothing has been published yet.
    }

    let deadline = TokioInstant::now() + CATCH_UP_DEADLINE;
    loop {
        match store.recorder_position().await {
            Ok(position) if position >= target => break,
            Ok(_) => {}
            Err(err) => warn!("QuestDB: Failed to read the recorder's position: {err}"),
        }
        if TokioInstant::now() >= deadline {
            return Err(AppError::RecorderBehind);
        }
        sleep(CATCH_UP_POLL).await;
    }

    // The position can land before the rows written ahead of it. Commits keep being made under
    // load, so this waits for the ones made by now rather than for the table to go quiet.
    let mut needed = None;
    loop {
        match store.orders_commits().await {
            Ok(commits) if commits.applied >= *needed.get_or_insert(commits.made) => return Ok(()),
            Ok(_) => {}
            Err(err) => warn!("QuestDB: Failed to read commit progress: {err}"),
        }
        if TokioInstant::now() >= deadline {
            return Err(AppError::RecorderBehind);
        }
        sleep(CATCH_UP_POLL).await;
    }
}

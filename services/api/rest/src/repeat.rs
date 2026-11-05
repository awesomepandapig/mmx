//! Bounces an order whose ID already reached the engine.
//!
//! The engine only knows the IDs of resting orders, so an order that filled or was cancelled could
//! run a second time if a game server retried it. Keeping the rule out of the engine means the
//! gateway has to find out from what's been published.
//!
//! A gateway reads every report, so it remembers each new order for a retry window, whichever
//! gateway sent it. Normally that memory covers the whole window and no request touches QuestDB.
//! After a start or a crash it doesn't yet, because reports published earlier were never read.
//! Until it does, each order is checked against QuestDB, once the recorder has passed the stream
//! position this gateway has reached. Anything after that position is in memory, so between them
//! the two leave no gap.
//!
//! A game server must not retry an order for longer than the window.

use super::errors::AppError;
use super::store::{OrderKey, OrderStore};

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use log::warn;

use tokio::time::{Instant as TokioInstant, sleep};

use uuid::Uuid;

const UNKNOWN_POSITION: i64 = -1;
const CATCH_UP_DEADLINE: Duration = Duration::from_secs(2);
const CATCH_UP_POLL: Duration = Duration::from_millis(20);

/// What the thread reading the report stream shares with request handlers.
pub struct ReportStream {
    started: Instant,
    position: AtomicI64,
    covered_since_ms: AtomicU64,
}

impl Default for ReportStream {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            position: AtomicI64::new(UNKNOWN_POSITION),
            covered_since_ms: AtomicU64::new(0),
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

    /// Records that every new order from `since` onwards is in memory, and earlier ones may not be.
    pub fn covered_since(&self, since: Instant) {
        let millis = since.saturating_duration_since(self.started).as_millis() as u64;
        self.covered_since_ms.fetch_max(millis, Ordering::AcqRel);
    }

    pub fn covers(&self, window: Duration) -> bool {
        let covered_since = self.covered_since_ms.load(Ordering::Acquire);
        let elapsed = self.started.elapsed().as_millis() as u64;
        elapsed.saturating_sub(covered_since) >= window.as_millis() as u64
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

pub struct RepeatGuard {
    pub stream: Arc<ReportStream>,
    pub store: Arc<dyn OrderStore>,
    pub window: Duration,
}

impl RepeatGuard {
    /// Checks an order against history. The gateway's memory is checked later, by the thread that
    /// owns it, so that the check can't race a report arriving.
    pub async fn check(&self, account: Uuid, cl_ord_id: Uuid) -> Result<(), AppError> {
        if self.stream.covers(self.window) {
            return Ok(());
        }
        await_recorder(&self.stream, self.store.as_ref()).await?;
        let key = OrderKey { account, cl_ord_id };
        match self.store.order_reached_engine(&key, self.window).await {
            Ok(true) => Err(AppError::RepeatOrder(cl_ord_id)),
            Ok(false) => Ok(()),
            Err(err) => {
                warn!("QuestDB: Failed to check for a repeat order: {err}");
                Err(AppError::RecorderBehind)
            }
        }
    }
}

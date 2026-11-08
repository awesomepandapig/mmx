//! Where reports come from: the tape in a cluster, or the live stream of a standalone engine,
//! which records nothing and so can't be resumed.

use std::thread::sleep;
use std::time::Duration;

use tracing::{error, warn};

use crate::follower::{Follower, TapeConfig};

const RETRY_AFTER: Duration = Duration::from_secs(1);

fn aeron_error_handler(code: i32, message: &str) {
    error!("Aeron: Error {}: {}", code, message);
}

fn subscribe() -> (transport::Aeron, transport::AeronSubscription) {
    loop {
        match transport::subscribe_to_executions(aeron_error_handler) {
            Ok(client) => return client,
            Err(e) => {
                error!("Aeron: Failed to subscribe: {:?}", e);
                sleep(RETRY_AFTER);
            }
        }
    }
}

pub enum Source {
    Live(transport::Aeron, transport::AeronSubscription),
    Tape(Follower),
}

impl Source {
    /// Connects to the media driver, waiting for as long as that takes.
    pub fn open(config: Option<TapeConfig>, position: i64) -> Self {
        let Some(config) = config else {
            let (aeron, subscription) = subscribe();
            return Self::Live(aeron, subscription);
        };
        loop {
            match Follower::new(config.clone(), position) {
                Ok(follower) => return Self::Tape(follower),
                Err(e) => {
                    error!("Aeron: Failed to connect: {}", e);
                    sleep(RETRY_AFTER);
                }
            }
        }
    }

    /// Delivers up to `limit` messages, each with the stream position that follows it.
    pub fn poll(&mut self, limit: usize, handler: &mut dyn FnMut(&[u8], i64)) -> i32 {
        match self {
            Self::Tape(follower) => follower.poll(limit, handler),
            Self::Live(aeron, subscription) => {
                let result = subscription
                    .poll_fn(|buffer, header| handler(buffer, header.position()), limit);
                match result {
                    Ok(read) if !aeron.is_closed() => read,
                    result => {
                        warn!("Aeron: Lost the subscription: {:?}", result);
                        let (aeron, subscription) = subscribe();
                        *self = Self::Live(aeron, subscription);
                        0
                    }
                }
            }
        }
    }

    /// The saved position to resume from, if this source can resume.
    pub fn position(&self) -> Option<i64> {
        match self {
            Self::Tape(follower) => Some(follower.position()),
            Self::Live(..) => None,
        }
    }

    /// Whether messages are arriving as they happen, rather than from history.
    pub fn is_live(&self) -> bool {
        match self {
            Self::Tape(follower) => follower.is_live(),
            Self::Live(..) => true,
        }
    }
}

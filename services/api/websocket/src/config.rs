use crate::auth::PlayerTokens;

use std::time::Duration;

use tracing::warn;

#[derive(Clone)]
pub struct Config {
    /// A socket closes after this long with no message in either direction.
    pub idle_timeout: Duration,
    pub heartbeat_interval: Duration,
    pub ticker_refresh: Duration,
    pub ticker_batch_interval: Duration,
    pub handshake_timeout: Duration,
    /// How long a client gets to accept one frame, and to answer a close.
    pub write_timeout: Duration,
    /// Frames waiting for a socket. A client that falls this far behind is closed, because a gap
    /// would silently corrupt its book.
    pub queue_capacity: usize,
    pub max_subscriptions: usize,
    /// Without this, the user channel refuses every subscription.
    pub player_tokens: Option<PlayerTokens>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(60),
            heartbeat_interval: Duration::from_secs(1),
            ticker_refresh: Duration::from_secs(1),
            ticker_batch_interval: Duration::from_secs(5),
            handshake_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(10),
            queue_capacity: 4096,
            max_subscriptions: 4096,
            player_tokens: None,
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let secret = std::env::var("WS_PLAYER_TOKEN_SECRET").unwrap_or_default();
        let player_tokens = PlayerTokens::new(secret.as_bytes());
        if player_tokens.is_none() {
            warn!(
                "WS_PLAYER_TOKEN_SECRET is missing or under 32 bytes, so the user channel is off"
            );
        }

        Self {
            player_tokens,
            ..Self::default()
        }
    }
}

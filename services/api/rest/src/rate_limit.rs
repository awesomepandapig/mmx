//! Per-player order limits, counted in Valkey so that every gateway shares them.
//!
//! Each player gets the default limit unless they have an override, which is a hash at
//! `mmx:rate:{<player>}:override` with `limit` and `window_ms` fields, either of them optional.
//! Set `limit` to 0 to stop a player trading. Overrides take effect on the next order.

use std::time::Duration;

use async_trait::async_trait;

use log::warn;

use redis::Script;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};

use tokio::sync::OnceCell;
use tokio::time::timeout;

use uuid::Uuid;

const CHECK_TIMEOUT: Duration = Duration::from_millis(100);

/// Counts orders in a fixed window. It runs atomically, so gateways can't race each other.
const CHECK_SCRIPT: &str = r"
local limit = tonumber(ARGV[1])
local window_ms = tonumber(ARGV[2])
local override = redis.call('HMGET', KEYS[2], 'limit', 'window_ms')
if override[1] then limit = tonumber(override[1]) end
if override[2] then window_ms = tonumber(override[2]) end
local count = redis.call('INCR', KEYS[1])
if count == 1 then redis.call('PEXPIRE', KEYS[1], window_ms) end
if count > limit then return {0, redis.call('PTTL', KEYS[1])} end
return {1, 0}
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit {
    pub orders: u64,
    pub window_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    Limited { retry_after_ms: u64 },
}

#[async_trait]
pub trait RateLimiter: Send + Sync {
    /// Counts one order. An error means the limit couldn't be checked.
    async fn check(&self, player: Uuid) -> Result<Decision, String>;
}

/// Used when no `VALKEY_URL` is set.
pub struct Unlimited;

#[async_trait]
impl RateLimiter for Unlimited {
    async fn check(&self, _player: Uuid) -> Result<Decision, String> {
        Ok(Decision::Allowed)
    }
}

pub fn count_key(player: Uuid) -> String {
    format!("mmx:rate:{{{player}}}:count")
}

pub fn override_key(player: Uuid) -> String {
    format!("mmx:rate:{{{player}}}:override")
}

fn decision(reply: &[i64]) -> Result<Decision, String> {
    match reply {
        [1, _] => Ok(Decision::Allowed),
        [0, retry_after_ms] => {
            Ok(Decision::Limited { retry_after_ms: (*retry_after_ms).max(1) as u64 })
        }
        other => Err(format!("unexpected reply {other:?}")),
    }
}

pub struct Valkey {
    client: redis::Client,
    connection: OnceCell<ConnectionManager>,
    script: Script,
    default_limit: Limit,
}

impl Valkey {
    pub fn new(url: &str, default_limit: Limit) -> Result<Self, String> {
        Ok(Self {
            client: redis::Client::open(url).map_err(|err| err.to_string())?,
            connection: OnceCell::new(),
            script: Script::new(CHECK_SCRIPT),
            default_limit,
        })
    }

    /// Connects on first use, so a gateway can start while Valkey is down.
    async fn connection(&self) -> Result<ConnectionManager, String> {
        let config = ConnectionManagerConfig::new()
            .set_connection_timeout(Some(CHECK_TIMEOUT))
            .set_response_timeout(Some(CHECK_TIMEOUT))
            .set_number_of_retries(1);
        self.connection
            .get_or_try_init(|| ConnectionManager::new_with_config(self.client.clone(), config))
            .await
            .cloned()
            .map_err(|err| err.to_string())
    }

    async fn count(&self, player: Uuid) -> Result<Decision, String> {
        let mut connection = self.connection().await?;
        let reply: Vec<i64> = self
            .script
            .key(count_key(player))
            .key(override_key(player))
            .arg(self.default_limit.orders)
            .arg(self.default_limit.window_ms)
            .invoke_async(&mut connection)
            .await
            .map_err(|err| err.to_string())?;
        decision(&reply)
    }
}

#[async_trait]
impl RateLimiter for Valkey {
    async fn check(&self, player: Uuid) -> Result<Decision, String> {
        timeout(CHECK_TIMEOUT, self.count(player))
            .await
            .map_err(|_| "timed out".to_string())
            .and_then(|result| result)
            .inspect_err(|err| warn!("Valkey: Rate limit check failed: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_share_a_hash_slot_per_player() {
        let player = Uuid::nil();
        let tag = format!("{{{player}}}");
        assert!(count_key(player).contains(&tag));
        assert!(override_key(player).contains(&tag));
        assert_ne!(count_key(player), override_key(player));
    }

    #[test]
    fn reads_the_script_reply() {
        assert_eq!(decision(&[1, 0]), Ok(Decision::Allowed));
        assert_eq!(decision(&[0, 250]), Ok(Decision::Limited { retry_after_ms: 250 }));
        // A key can expire between the count and the PTTL, which reports a negative time.
        assert_eq!(decision(&[0, -2]), Ok(Decision::Limited { retry_after_ms: 1 }));
        assert!(decision(&[]).is_err());
    }
}

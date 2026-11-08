//! Per-player order limits and order ID claims, in Valkey so that every gateway shares them.
//!
//! Each player gets the default limit unless they have an override, which is a hash at
//! `mmx:rate:{<player>}:override` with `limit` and `window_ms` fields, either of them optional.
//! Set `limit` to 0 to stop a player trading. Overrides take effect on the next order.
//!
//! The engine forgets an order ID once the order ends, so a replayed POST would run as a new order.
//! In cluster mode each order must first claim its ID at `mmx:claim:{<player>}:<id>`, and a claim
//! that isn't confirmed means the order isn't sent. The claim and the limit are one script, so no
//! two gateways can both pass, and a repeat stops before it's counted. Valkey's clock decides
//! whether an ID is in date, so a claim's lifetime depends on one clock rather than every gateway's.
//!
//! Valkey has no persistence, so an empty one looks like "nothing was claimed". The script stamps
//! `mmx:claims:epoch` with the time it found the marker missing, and refuses claims until one full
//! hold has passed, by when no lost claim's ID can be accepted. The stamp lives in Valkey, so every
//! gateway sees the same hold, including one that starts halfway through it. A first install can't
//! be told apart from a loss, so it waits too.

use super::order_id::{FUTURE_SKEW, OrderIdRules};

use std::time::Duration;

use async_trait::async_trait;

use log::warn;

use redis::Script;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};

use tokio::sync::OnceCell;
use tokio::time::timeout;

use uuid::Uuid;

pub const CHECK_TIMEOUT: Duration = Duration::from_millis(100);

/// Counts orders in a fixed window, after any claim. It runs atomically, so gateways can't race
/// each other. A limited order gives its claim back, because nothing was sent under it.
///
/// KEYS: count, override, then for a claim, the epoch and the claim itself. The epoch alone is the
/// claim store's health check. It's the one key outside the player's hash tag, so splitting Valkey
/// into shards will need an epoch per shard.
const CHECK_SCRIPT: &str = r"
local limit = tonumber(ARGV[1])
local window_ms = tonumber(ARGV[2])
if KEYS[3] then
  local time = redis.call('TIME')
  local now = time[1] * 1000 + math.floor(time[2] / 1000)
  local since = tonumber(redis.call('GET', KEYS[3]))
  if not since then
    since = now
    redis.call('SET', KEYS[3], string.format('%.0f', now))
  end
  local hold = since + tonumber(ARGV[3]) - now
  if hold > 0 then return {3, hold} end
  if not KEYS[4] then return {1, 0} end
  local id_ms = tonumber(ARGV[7])
  if id_ms < now - tonumber(ARGV[5]) or id_ms > now + tonumber(ARGV[6]) then return {4, 0} end
  if not redis.call('SET', KEYS[4], 1, 'NX', 'PX', ARGV[4]) then return {2, 0} end
end
local override = redis.call('HMGET', KEYS[2], 'limit', 'window_ms')
if override[1] then limit = tonumber(override[1]) end
if override[2] then window_ms = tonumber(override[2]) end
local count = redis.call('INCR', KEYS[1])
if count == 1 then redis.call('PEXPIRE', KEYS[1], window_ms) end
if count > limit then
  if KEYS[4] then redis.call('DEL', KEYS[4]) end
  return {0, redis.call('PTTL', KEYS[1])}
end
return {1, 0}
";

const EPOCH_KEY: &str = "mmx:claims:epoch";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit {
    pub orders: u64,
    pub window_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    Limited {
        retry_after_ms: u64,
    },
    /// Another request already claimed this order ID.
    Duplicate,
    /// The claim store lost its data, so nothing can be claimed yet.
    Recovering {
        retry_after_ms: u64,
    },
    /// By Valkey's clock this ID is out of date, though it wasn't by the gateway's.
    OutOfDate,
}

/// An order ID to claim, and the time inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Claim {
    pub id: Uuid,
    pub time_ms: u64,
}

#[async_trait]
pub trait RateLimiter: Send + Sync {
    /// Claims the order's ID, if it has one to claim, and counts the order. An error means neither
    /// is confirmed. The claim may still have landed, which only makes the ID unusable.
    async fn check(&self, player: Uuid, claim: Option<Claim>) -> Result<Decision, String>;

    /// Whether a claim could be made now, without making one. An error means Valkey can't be read.
    async fn claims_ready(&self) -> Result<Decision, String> {
        Ok(Decision::Allowed)
    }
}

/// Used when no `VALKEY_URL` is set.
pub struct Unlimited;

#[async_trait]
impl RateLimiter for Unlimited {
    async fn check(&self, _player: Uuid, _claim: Option<Claim>) -> Result<Decision, String> {
        Ok(Decision::Allowed)
    }
}

pub fn count_key(player: Uuid) -> String {
    format!("mmx:rate:{{{player}}}:count")
}

pub fn override_key(player: Uuid) -> String {
    format!("mmx:rate:{{{player}}}:override")
}

pub fn claim_key(player: Uuid, id: Uuid) -> String {
    format!("mmx:claim:{{{player}}}:{id}")
}

fn decision(reply: &[i64]) -> Result<Decision, String> {
    match reply {
        [1, _] => Ok(Decision::Allowed),
        [0, retry_after_ms] => Ok(Decision::Limited {
            retry_after_ms: (*retry_after_ms).max(1) as u64,
        }),
        [2, _] => Ok(Decision::Duplicate),
        [3, retry_after_ms] => Ok(Decision::Recovering {
            retry_after_ms: (*retry_after_ms).max(1) as u64,
        }),
        [4, _] => Ok(Decision::OutOfDate),
        other => Err(format!("unexpected reply {other:?}")),
    }
}

/// Connects on first use, so a gateway can start while Valkey is down.
pub struct ValkeyLink {
    client: redis::Client,
    connection: OnceCell<ConnectionManager>,
}

impl ValkeyLink {
    pub fn new(url: &str) -> Result<Self, String> {
        let client = redis::Client::open(url).map_err(|err| err.to_string())?;
        Ok(Self {
            client,
            connection: OnceCell::new(),
        })
    }

    pub async fn connection(&self) -> Result<ConnectionManager, String> {
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
}

pub struct Valkey {
    link: ValkeyLink,
    script: Script,
    default_limit: Limit,
    /// Without these, as outside cluster mode, nothing can be claimed.
    id_rules: Option<OrderIdRules>,
}

impl Valkey {
    pub fn new(
        url: &str,
        default_limit: Limit,
        id_rules: Option<OrderIdRules>,
    ) -> Result<Self, String> {
        let link = ValkeyLink::new(url)?;
        Ok(Self {
            link,
            script: Script::new(CHECK_SCRIPT),
            default_limit,
            id_rules,
        })
    }

    async fn run(
        &self,
        player: Uuid,
        claim: Option<Claim>,
        epoch: bool,
    ) -> Result<Decision, String> {
        let mut connection = self.link.connection().await?;
        let mut call = self.script.prepare_invoke();
        call.key(count_key(player))
            .key(override_key(player))
            .arg(self.default_limit.orders)
            .arg(self.default_limit.window_ms);
        if epoch {
            let rules = self.id_rules.ok_or("claims need the order ID rules")?;
            call.key(EPOCH_KEY)
                .arg(rules.recovery_hold().as_millis() as u64)
                .arg(rules.claim_ttl().as_millis() as u64)
                .arg(rules.window.as_millis() as u64)
                .arg(FUTURE_SKEW.as_millis() as u64);
        }
        if let Some(claim) = claim {
            call.key(claim_key(player, claim.id)).arg(claim.time_ms);
        }
        let reply: Vec<i64> = call
            .invoke_async(&mut connection)
            .await
            .map_err(|err| err.to_string())?;
        decision(&reply)
    }

    async fn limited(
        &self,
        player: Uuid,
        claim: Option<Claim>,
        epoch: bool,
    ) -> Result<Decision, String> {
        timeout(CHECK_TIMEOUT, self.run(player, claim, epoch))
            .await
            .map_err(|_| "timed out".to_string())
            .and_then(|result| result)
            .inspect_err(|err| warn!("Valkey: Order check failed: {err}"))
    }
}

#[async_trait]
impl RateLimiter for Valkey {
    async fn check(&self, player: Uuid, claim: Option<Claim>) -> Result<Decision, String> {
        self.limited(player, claim, claim.is_some()).await
    }

    async fn claims_ready(&self) -> Result<Decision, String> {
        self.limited(Uuid::nil(), None, true).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    #[test]
    fn keys_share_a_hash_slot_per_player() {
        let player = Uuid::nil();
        let tag = format!("{{{player}}}");
        assert!(count_key(player).contains(&tag));
        assert!(override_key(player).contains(&tag));
        assert!(claim_key(player, Uuid::max()).contains(&tag));
        assert_ne!(count_key(player), override_key(player));
    }

    #[test]
    fn reads_the_script_reply() {
        assert_eq!(decision(&[1, 0]), Ok(Decision::Allowed));
        assert_eq!(
            decision(&[0, 250]),
            Ok(Decision::Limited {
                retry_after_ms: 250
            })
        );
        // A key can expire between the count and the PTTL, which reports a negative time.
        assert_eq!(
            decision(&[0, -2]),
            Ok(Decision::Limited { retry_after_ms: 1 })
        );
        assert_eq!(decision(&[2, 0]), Ok(Decision::Duplicate));
        assert_eq!(
            decision(&[3, 500]),
            Ok(Decision::Recovering {
                retry_after_ms: 500
            })
        );
        assert_eq!(decision(&[4, 0]), Ok(Decision::OutOfDate));
        assert!(decision(&[]).is_err());
    }

    const RULES: OrderIdRules = OrderIdRules {
        window: Duration::from_secs(2),
        session_timeout: Duration::from_secs(1),
    };
    const LIMIT: Limit = Limit {
        orders: 2,
        window_ms: 60_000,
    };

    async fn raw(client: &redis::Client, command: redis::Cmd) -> redis::Value {
        let mut connection = client.get_multiplexed_async_connection().await.unwrap();
        command.query_async(&mut connection).await.unwrap()
    }

    fn claim_now() -> Claim {
        let id = Uuid::now_v7();
        Claim {
            id,
            time_ms: crate::order_id::time_ms(id).unwrap(),
        }
    }

    /// Run by hand against a scratch server, because it empties it:
    /// `CLAIMS_TEST_VALKEY_URL=redis://localhost:6390 cargo test -p api real_valkey -- --ignored`
    #[tokio::test]
    #[ignore = "needs a scratch Valkey"]
    async fn real_valkey_claims_once_and_holds_after_a_loss() {
        let url = std::env::var("CLAIMS_TEST_VALKEY_URL").unwrap();
        let client = redis::Client::open(url.as_str()).unwrap();
        raw(&client, redis::cmd("FLUSHALL")).await;
        let valkey = Arc::new(Valkey::new(&url, LIMIT, Some(RULES)).unwrap());
        let other = Arc::new(Valkey::new(&url, LIMIT, Some(RULES)).unwrap());
        let player = Uuid::now_v7();

        // An empty store is a lost one, so the first caller starts the hold for everyone.
        let held = valkey.check(player, Some(claim_now())).await.unwrap();
        assert!(matches!(held, Decision::Recovering { .. }), "{held:?}");
        assert!(matches!(
            other.claims_ready().await,
            Ok(Decision::Recovering { .. })
        ));
        let late_start = Valkey::new(&url, LIMIT, Some(RULES)).unwrap();
        let Ok(Decision::Recovering { retry_after_ms }) = late_start.claims_ready().await else {
            panic!("a gateway starting now must join the same hold");
        };
        assert!(retry_after_ms <= RULES.recovery_hold().as_millis() as u64);
        tokio::time::sleep(RULES.recovery_hold()).await;
        assert_eq!(valkey.claims_ready().await, Ok(Decision::Allowed));

        // Many callers at once, across two connections, and exactly one claims.
        let claim = claim_now();
        let calls: Vec<_> = (0..32)
            .map(|turn| {
                let gateway = if turn % 2 == 0 {
                    valkey.clone()
                } else {
                    other.clone()
                };
                tokio::spawn(async move { gateway.run(player, Some(claim), true).await })
            })
            .collect();
        let mut answers = Vec::new();
        for call in calls {
            answers.push(call.await.unwrap());
        }
        let won = answers
            .iter()
            .filter(|answer| **answer == Ok(Decision::Allowed))
            .count();
        let lost = answers
            .iter()
            .filter(|answer| **answer == Ok(Decision::Duplicate))
            .count();
        assert_eq!((won, lost), (1, 31), "{answers:?}");

        // The repeats spent nothing: one more order fits in a limit of two, and the next doesn't.
        assert_eq!(
            valkey.check(player, Some(claim_now())).await,
            Ok(Decision::Allowed)
        );
        let refused = claim_now();
        let limited = valkey.check(player, Some(refused)).await.unwrap();
        assert!(matches!(limited, Decision::Limited { .. }), "{limited:?}");
        let exists = raw(
            &client,
            redis::cmd("EXISTS")
                .arg(claim_key(player, refused.id))
                .clone(),
        )
        .await;
        assert_eq!(
            exists,
            redis::Value::Int(0),
            "a limited order keeps nothing"
        );

        // The claim's lifetime is the one it was promised.
        let key = claim_key(player, claim.id);
        let redis::Value::Int(ttl) = raw(&client, redis::cmd("PTTL").arg(&key).clone()).await
        else {
            panic!("expected a TTL");
        };
        assert!(
            ttl > RULES.window.as_millis() as i64 + FUTURE_SKEW.as_millis() as i64,
            "{ttl}"
        );

        // Valkey's clock has the last word on an ID's age.
        let old = Claim {
            id: Uuid::now_v7(),
            time_ms: 1,
        };
        assert_eq!(
            valkey.check(Uuid::now_v7(), Some(old)).await,
            Ok(Decision::OutOfDate)
        );
        let ahead = Claim {
            id: Uuid::now_v7(),
            time_ms: u64::MAX / 2,
        };
        assert_eq!(
            valkey.check(Uuid::now_v7(), Some(ahead)).await,
            Ok(Decision::OutOfDate)
        );

        // Losing everything starts a new hold, and a replay of a lost ID can't pass during it.
        raw(&client, redis::cmd("FLUSHALL")).await;
        let replay = valkey.check(player, Some(claim)).await.unwrap();
        assert!(matches!(replay, Decision::Recovering { .. }), "{replay:?}");
        tokio::time::sleep(RULES.recovery_hold()).await;
        // By now the ID is out of date, so the lost claim can't matter.
        assert_eq!(
            valkey.check(player, Some(claim)).await,
            Ok(Decision::OutOfDate)
        );
    }

    #[tokio::test]
    async fn a_dead_valkey_is_an_error_inside_the_timeout() {
        // Port 1 has nothing listening, so this is the real client against a refused connection.
        let valkey = Valkey::new("redis://127.0.0.1:1", LIMIT, Some(RULES)).unwrap();
        let started = std::time::Instant::now();
        assert!(valkey.check(Uuid::nil(), Some(claim_now())).await.is_err());
        assert!(valkey.claims_ready().await.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    }
}

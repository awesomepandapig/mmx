//! What makes a client order ID usable. In cluster mode every ID is a UUIDv7, so it carries the time
//! it was made, and that time bounds everything else:
//!
//! - An ID is only accepted for `window` after its time, so a repeat can only arrive in that span.
//! - A claim outlives the span, so an ID can't be claimed twice. See `rate_limit.rs`.
//! - An order can only land for a bounded time after its ID's, so an ID old enough that isn't on the
//!   tape never will be. See `settled_after`.

use super::aeron_handler::REQUEST_DEADLINE;
use super::errors::AppError;

use std::time::Duration;

use uuid::{Uuid, Variant, Version};

pub const DEFAULT_WINDOW_SECS: u64 = 60;
/// How far a caller's clock may run ahead of ours.
pub const FUTURE_SKEW: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
pub struct OrderIdRules {
    /// How long after its time an ID is still accepted.
    pub window: Duration,
    /// How long the cluster keeps a silent session. It must match the nodes' setting.
    pub session_timeout: Duration,
}

/// When a UUIDv7 was made, in Unix milliseconds. Any other kind of ID has no usable time.
pub fn time_ms(id: Uuid) -> Option<u64> {
    let is_v7 = id.get_version() == Some(Version::SortRand) && id.get_variant() == Variant::RFC4122;
    let (seconds, nanos) = id.get_timestamp().filter(|_| is_v7)?.to_unix();
    Some(seconds * 1000 + u64::from(nanos) / 1_000_000)
}

impl OrderIdRules {
    pub fn check(&self, id: Uuid, now_ms: u64) -> Result<(), AppError> {
        let time = time_ms(id).ok_or(AppError::InvalidOrderId(id))?;
        if time.saturating_add(self.window.as_millis() as u64) < now_ms {
            return Err(AppError::StaleOrderId(id));
        }
        if time > now_ms.saturating_add(FUTURE_SKEW.as_millis() as u64) {
            return Err(AppError::FutureOrderId(id));
        }
        Ok(())
    }

    /// An ID dated `FUTURE_SKEW` ahead stays acceptable for this long after it's first seen, so this
    /// is the shortest a claim can safely live.
    fn accepted_for(&self) -> Duration {
        self.window + FUTURE_SKEW
    }

    /// Twice what's needed, so a claim can't expire while its ID is acceptable, even if `window` is
    /// raised on one gateway before the others.
    pub fn claim_ttl(&self) -> Duration {
        self.accepted_for() * 2
    }

    /// How long new orders stop after the claim store loses its data. A lost claim is harmless once
    /// its ID can't be accepted. The extra skew is for a Valkey that returns on another machine,
    /// whose clock differs from the one that took the claims.
    pub fn recovery_hold(&self) -> Duration {
        self.accepted_for() + FUTURE_SKEW
    }

    /// How old an ID must be before an order under it can't still land. It can arrive at the very
    /// end of its window, the gateway may send it until `REQUEST_DEADLINE`, and once sent it can only
    /// land while the gateway's session lives. `FUTURE_SKEW` covers this gateway's clock running
    /// ahead of the one that accepted it.
    pub fn settled_after(&self) -> Duration {
        self.accepted_for() + REQUEST_DEADLINE + self.session_timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_MS: u64 = 1_800_000_000_000;

    fn rules() -> OrderIdRules {
        OrderIdRules {
            window: Duration::from_secs(DEFAULT_WINDOW_SECS),
            session_timeout: Duration::from_secs(30),
        }
    }

    fn id_at(ms: u64) -> Uuid {
        let time = uuid::Timestamp::from_unix(uuid::NoContext, ms / 1000, (ms % 1000) as u32 * 1_000_000);
        Uuid::new_v7(time)
    }

    fn check(id: Uuid) -> String {
        match rules().check(id, NOW_MS) {
            Ok(()) => "ok".to_string(),
            Err(err) => format!("{err:?}"),
        }
    }

    #[test]
    fn reads_the_time_inside_an_id() {
        assert_eq!(time_ms(id_at(NOW_MS)), Some(NOW_MS));
        assert_eq!(time_ms(Uuid::new_v4()), None);
        assert_eq!(time_ms(Uuid::nil()), None);
        assert_eq!(time_ms(Uuid::max()), None);
        // Version 7 bits with the wrong variant, so not an RFC 9562 ID.
        let id = Uuid::from_u128(id_at(NOW_MS).as_u128() & !(0b11 << 62));
        assert_eq!(time_ms(id), None);
    }

    #[test]
    fn accepts_the_window_and_its_edges() {
        let window = rules().window.as_millis() as u64;
        let skew = FUTURE_SKEW.as_millis() as u64;
        assert_eq!(check(id_at(NOW_MS)), "ok");
        assert_eq!(check(id_at(NOW_MS - window)), "ok");
        assert_eq!(check(id_at(NOW_MS + skew)), "ok");
        assert!(check(id_at(NOW_MS - window - 1)).contains("Stale"));
        assert!(check(id_at(NOW_MS + skew + 1)).contains("Future"));
    }

    #[test]
    fn other_kinds_of_id_are_refused() {
        assert!(check(Uuid::new_v4()).contains("InvalidOrderId"));
        assert!(check(Uuid::nil()).contains("InvalidOrderId"));
    }

    #[test]
    fn a_claim_outlives_every_id_that_could_repeat_it() {
        for window in [1, 60, 3600] {
            let rules = OrderIdRules { window: Duration::from_secs(window), ..rules() };
            assert!(rules.claim_ttl() >= rules.window + FUTURE_SKEW);
            assert!(rules.recovery_hold() >= rules.window + FUTURE_SKEW);
        }
        assert_eq!(rules().claim_ttl(), Duration::from_secs(130));
        assert_eq!(rules().recovery_hold(), Duration::from_secs(70));
    }

    /// An order can arrive at the end of its window, so the session timeout alone isn't enough.
    #[test]
    fn nothing_is_settled_while_its_id_can_still_be_accepted() {
        let rules = rules();
        assert!(rules.settled_after() > rules.window + FUTURE_SKEW + rules.session_timeout);
        assert_eq!(rules.settled_after(), Duration::from_secs(98));
    }
}

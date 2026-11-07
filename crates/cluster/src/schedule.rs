//! Decides when the leader asks for a snapshot, using only what the log says. It reads no clock,
//! and an idle log never comes due, because nothing has changed since the last snapshot.

/// A limit of 0 turns that trigger off.
#[derive(Debug, Clone, Copy)]
pub struct SnapshotPolicy {
    /// Bytes of log since the last snapshot, which bounds replay time after a restart.
    pub log_bytes: i64,
    /// Cluster time since the last snapshot, in the units of the cluster's clock.
    pub interval: i64,
}

pub(crate) struct SnapshotSchedule {
    policy: SnapshotPolicy,
    last_position: i64,
    last_time: i64,
}

impl SnapshotSchedule {
    pub(crate) const fn new(policy: SnapshotPolicy, log_position: i64, cluster_time: i64) -> Self {
        Self {
            policy,
            last_position: log_position,
            last_time: cluster_time,
        }
    }

    pub(crate) const fn is_due(&self, log_position: i64, cluster_time: i64) -> bool {
        let grown = log_position - self.last_position;
        let by_size = self.policy.log_bytes > 0 && grown >= self.policy.log_bytes;
        let by_time = self.policy.interval > 0
            && grown > 0
            && cluster_time - self.last_time >= self.policy.interval;
        by_size || by_time
    }

    /// Every node sees the snapshot at the same log position, so any of them can take over the
    /// schedule without moving it.
    pub(crate) const fn on_snapshot(&mut self, log_position: i64, cluster_time: i64) {
        self.last_position = log_position;
        self.last_time = cluster_time;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule(log_bytes: i64, interval: i64) -> SnapshotSchedule {
        SnapshotSchedule::new(
            SnapshotPolicy {
                log_bytes,
                interval,
            },
            1_000,
            5_000,
        )
    }

    #[test]
    fn comes_due_on_log_growth() {
        let schedule = schedule(100, 0);
        assert!(!schedule.is_due(1_099, i64::MAX));
        assert!(schedule.is_due(1_100, 5_000));
    }

    #[test]
    fn comes_due_on_cluster_time() {
        let schedule = schedule(0, 60);
        assert!(!schedule.is_due(1_001, 5_059));
        assert!(schedule.is_due(1_001, 5_060));
    }

    #[test]
    fn an_idle_log_never_comes_due() {
        assert!(!schedule(100, 60).is_due(1_000, i64::MAX));
    }

    #[test]
    fn zero_turns_a_trigger_off() {
        assert!(!schedule(0, 0).is_due(i64::MAX, i64::MAX));
    }

    #[test]
    fn a_snapshot_restarts_both_triggers() {
        let mut schedule = schedule(100, 60);
        assert!(schedule.is_due(1_100, 5_000));
        schedule.on_snapshot(1_100, 5_000);
        assert!(!schedule.is_due(1_199, 5_059));
    }
}

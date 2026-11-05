//! Which markets operators have halted, shared by every gateway through Valkey.
//!
//! Halts are a hash at `mmx:market_halts`, with one field per halted symbol. Each gateway polls it,
//! and the one answering an admin call applies the change at once. Polling needs no reconnect logic,
//! and a missed message can't leave a gateway wrong for longer than one poll.
//!
//! A halted market must never reopen by accident, so three things are handled on purpose:
//!
//! - **Valkey is unreachable.** A gateway keeps what it last read. A gateway that has never read the
//!   list knows nothing, so it refuses every new order until it can. Cancels don't come through here.
//! - **Valkey comes back empty.** Without persistence a restart loses the hash, and an empty hash
//!   looks like "nothing is halted". The hash also holds an epoch, so a missing epoch means the list
//!   was lost. Gateways then write back what they know instead of believing it.
//! - **A gateway rebuilt the list knowing less than this one.** A changed epoch can't be trusted to
//!   clear anything, so this gateway adds its own halts back.
//!
//! All of this can only keep a market halted. A resume might need repeating, which is the safe way
//! round. If every gateway restarts after Valkey lost the hash, nothing remembers the halts, and that
//! can't be told apart from a first install.

use super::rate_limit::{CHECK_TIMEOUT, ValkeyLink};

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use async_trait::async_trait;

use log::{error, info, warn};

use tokio::time::{sleep, timeout};

use uuid::Uuid;

/// With the store's 100 ms timeout, a change reaches a gateway within about 200 ms.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

const HALTS_KEY: &str = "mmx:market_halts";
/// Symbols are alphanumeric, so this field can't collide with one.
const EPOCH_FIELD: &str = "_epoch";
const HALTED_VALUE: &str = "1";

/// The shared list as one read saw it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Set once by whichever gateway finds it missing. Missing means the list is new or lost.
    pub epoch: Option<String>,
    pub halted: HashSet<String>,
}

#[async_trait]
pub trait HaltStore: Send + Sync {
    async fn read(&self) -> Result<Snapshot, String>;
    async fn claim_epoch(&self, epoch: &str) -> Result<(), String>;
    async fn halt(&self, symbol: &str) -> Result<(), String>;
    async fn resume(&self, symbol: &str) -> Result<(), String>;
}

fn snapshot(mut fields: HashMap<String, String>) -> Snapshot {
    let epoch = fields.remove(EPOCH_FIELD);
    Snapshot { epoch, halted: fields.into_keys().collect() }
}

pub struct ValkeyHalts {
    link: ValkeyLink,
}

impl ValkeyHalts {
    pub fn new(url: &str) -> Result<Self, String> {
        Ok(Self { link: ValkeyLink::new(url)? })
    }

    async fn limited<T>(call: impl Future<Output = Result<T, String>>) -> Result<T, String> {
        timeout(CHECK_TIMEOUT, call).await.map_err(|_| "timed out".to_string()).and_then(|result| result)
    }

    async fn run<T: redis::FromRedisValue>(&self, command: redis::Cmd) -> Result<T, String> {
        Self::limited(async {
            let mut connection = self.link.connection().await?;
            command.query_async(&mut connection).await.map_err(|err| err.to_string())
        })
        .await
    }
}

#[async_trait]
impl HaltStore for ValkeyHalts {
    async fn read(&self) -> Result<Snapshot, String> {
        let mut command = redis::cmd("HGETALL");
        command.arg(HALTS_KEY);
        self.run(command).await.map(snapshot)
    }

    async fn claim_epoch(&self, epoch: &str) -> Result<(), String> {
        let mut command = redis::cmd("HSETNX");
        command.arg(HALTS_KEY).arg(EPOCH_FIELD).arg(epoch);
        self.run::<i64>(command).await.map(|_| ())
    }

    async fn halt(&self, symbol: &str) -> Result<(), String> {
        let mut command = redis::cmd("HSET");
        command.arg(HALTS_KEY).arg(symbol).arg(HALTED_VALUE);
        self.run::<i64>(command).await.map(|_| ())
    }

    async fn resume(&self, symbol: &str) -> Result<(), String> {
        let mut command = redis::cmd("HDEL");
        command.arg(HALTS_KEY).arg(symbol);
        self.run::<i64>(command).await.map(|_| ())
    }
}

/// Why a new order can't go through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Halted,
    /// This gateway has never been able to read the list.
    Unknown,
}

struct State {
    /// `None` until the first read. Every market counts as halted until then.
    halted: Option<HashSet<String>>,
    /// Halts applied here that the store may not have yet. They outrank anything read from it.
    unshared: HashSet<String>,
    epoch: Option<String>,
    /// Counts local changes, so that a read which raced one is thrown away.
    changes: u64,
    store_healthy: bool,
}

/// Folds a read into the state. Returns an epoch to claim if the list has none.
fn apply(state: &mut State, snapshot: Snapshot) -> Option<String> {
    let Snapshot { epoch, halted: mut shared } = snapshot;

    // Nothing changes yet. The new epoch makes the next read restore what this gateway knows.
    let Some(epoch) = epoch else {
        if let Some(known) = state.halted.as_ref().filter(|known| !known.is_empty()) {
            error!("Halts: The shared list was lost, so restoring {known:?}");
        }
        return Some(Uuid::new_v4().to_string());
    };

    if state.epoch.as_ref().is_some_and(|known| *known != epoch)
        && let Some(known) = &state.halted
    {
        state.unshared.extend(known.difference(&shared).cloned());
    }
    shared.extend(state.unshared.iter().cloned());
    state.halted = Some(shared);
    state.epoch = Some(epoch);
    None
}

pub struct Halts {
    store: Option<Arc<dyn HaltStore>>,
    state: RwLock<State>,
}

impl Halts {
    /// A gateway with a shared store knows nothing until its first read.
    pub fn shared(store: Arc<dyn HaltStore>) -> Self {
        Self::build(Some(store), None)
    }

    /// With no store there's nothing to wait for, but halts last only as long as this process.
    pub fn standalone() -> Self {
        Self::build(None, Some(HashSet::new()))
    }

    fn build(store: Option<Arc<dyn HaltStore>>, halted: Option<HashSet<String>>) -> Self {
        let state = State {
            halted,
            unshared: HashSet::new(),
            epoch: None,
            changes: 0,
            store_healthy: true,
        };
        Self { store, state: RwLock::new(state) }
    }

    fn read_state(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_state(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn check(&self, symbol: &str) -> Result<(), Refusal> {
        match &self.read_state().halted {
            None => Err(Refusal::Unknown),
            Some(halted) if halted.contains(symbol) => Err(Refusal::Halted),
            Some(_) => Ok(()),
        }
    }

    /// Whether new orders for this market are refused here, for either reason.
    pub fn is_halted(&self, symbol: &str) -> bool {
        self.check(symbol).is_err()
    }

    /// Whether this gateway has a halt list to go on, even a stale one.
    pub fn is_known(&self) -> bool {
        self.read_state().halted.is_some()
    }

    /// Whether the last call to the store worked, if there is a store.
    pub fn store_healthy(&self) -> Option<bool> {
        self.store.as_ref().map(|_| self.read_state().store_healthy)
    }

    /// Applies a halt here at once. An error means it's in force on this gateway only, and it keeps
    /// being offered to the store until it lands.
    pub async fn halt(&self, symbol: &str) -> Result<(), String> {
        {
            let mut state = self.write_state();
            state.changes += 1;
            state.unshared.insert(symbol.to_string());
            if let Some(halted) = &mut state.halted {
                halted.insert(symbol.to_string());
            }
        }
        if let Some(store) = &self.store {
            store.halt(symbol).await?;
        }
        let mut state = self.write_state();
        state.changes += 1;
        state.unshared.remove(symbol);
        Ok(())
    }

    /// Resumes in the store first. If that fails the market stays halted everywhere, this gateway
    /// included, since the other way round would reopen it here and nowhere else.
    pub async fn resume(&self, symbol: &str) -> Result<(), String> {
        if let Some(store) = &self.store {
            store.resume(symbol).await?;
        }
        let mut state = self.write_state();
        state.changes += 1;
        state.unshared.remove(symbol);
        if let Some(halted) = &mut state.halted {
            halted.remove(symbol);
        }
        Ok(())
    }

    /// One poll. Local halts go up first, so that the read can't take them away.
    pub async fn refresh(&self) {
        let Some(store) = &self.store else { return };
        let result = self.sync(store.as_ref()).await;

        let mut state = self.write_state();
        let was_healthy = state.store_healthy;
        state.store_healthy = result.is_ok();
        match (&result, state.halted.as_ref()) {
            (Err(err), Some(halted)) if was_healthy => {
                error!("Halts: Valkey is unreachable, keeping {halted:?}: {err}");
            }
            (Err(err), None) if was_healthy => {
                error!("Halts: Valkey is unreachable and no halt list was ever read, so new orders are refused: {err}");
            }
            (Ok(()), _) if !was_healthy => info!("Halts: Valkey is reachable again"),
            _ => {}
        }
    }

    async fn sync(&self, store: &dyn HaltStore) -> Result<(), String> {
        let unshared: Vec<String> = self.read_state().unshared.iter().cloned().collect();
        for symbol in unshared {
            store.halt(&symbol).await?;
            self.write_state().unshared.remove(&symbol);
        }

        if let Some(epoch) = self.read_once(store).await? {
            store.claim_epoch(&epoch).await?;
            self.read_once(store).await?;
        }
        Ok(())
    }

    async fn read_once(&self, store: &dyn HaltStore) -> Result<Option<String>, String> {
        let changes = self.read_state().changes;
        let snapshot = store.read().await?;
        let mut state = self.write_state();
        if state.changes != changes {
            return Ok(None); // This read may predate a local change. The next poll won't.
        }
        Ok(apply(&mut state, snapshot))
    }

    pub fn start_polling(self: &Arc<Self>) {
        if self.store.is_none() {
            warn!("Halts: VALKEY_URL isn't set, so halts stay on this gateway and end with it");
            return;
        }
        let halts = self.clone();
        tokio::spawn(async move {
            loop {
                sleep(POLL_INTERVAL).await;
                halts.refresh().await;
            }
        });
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Valkey with a switch to take it down, and a way to lose everything.
    #[derive(Default)]
    pub struct FakeHaltStore {
        pub snapshot: Mutex<Snapshot>,
        pub down: AtomicBool,
    }

    impl FakeHaltStore {
        pub fn set_down(&self, down: bool) {
            self.down.store(down, Ordering::Relaxed);
        }

        pub fn lose_everything(&self) {
            *self.snapshot.lock().unwrap() = Snapshot::default();
        }

        pub fn holds(&self, symbol: &str) -> bool {
            self.snapshot.lock().unwrap().halted.contains(symbol)
        }

        fn up(&self) -> Result<(), String> {
            if self.down.load(Ordering::Relaxed) { Err("down".to_string()) } else { Ok(()) }
        }
    }

    #[async_trait]
    impl HaltStore for FakeHaltStore {
        async fn read(&self) -> Result<Snapshot, String> {
            self.up()?;
            Ok(self.snapshot.lock().unwrap().clone())
        }

        async fn claim_epoch(&self, epoch: &str) -> Result<(), String> {
            self.up()?;
            self.snapshot.lock().unwrap().epoch.get_or_insert_with(|| epoch.to_string());
            Ok(())
        }

        async fn halt(&self, symbol: &str) -> Result<(), String> {
            self.up()?;
            self.snapshot.lock().unwrap().halted.insert(symbol.to_string());
            Ok(())
        }

        async fn resume(&self, symbol: &str) -> Result<(), String> {
            self.up()?;
            self.snapshot.lock().unwrap().halted.remove(symbol);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeHaltStore;
    use super::*;

    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio::sync::Notify;

    fn store() -> Arc<FakeHaltStore> {
        Arc::new(FakeHaltStore::default())
    }

    async fn started(store: &Arc<FakeHaltStore>) -> Halts {
        let halts = Halts::shared(store.clone());
        halts.refresh().await;
        halts
    }

    #[test]
    fn reads_the_hash_with_the_epoch_apart_from_the_symbols() {
        let fields = HashMap::from([
            (EPOCH_FIELD.to_string(), "e1".to_string()),
            ("ITEM".to_string(), HALTED_VALUE.to_string()),
        ]);
        let read = snapshot(fields);
        assert_eq!(read.epoch.as_deref(), Some("e1"));
        assert_eq!(read.halted, HashSet::from(["ITEM".to_string()]));
    }

    #[test]
    fn the_epoch_field_cannot_be_a_symbol() {
        assert!(super::super::markets::normalize_symbol(EPOCH_FIELD).is_err());
    }

    #[tokio::test]
    async fn a_first_start_on_an_empty_store_is_open_and_claims_an_epoch() {
        let store = store();
        let halts = started(&store).await;
        assert_eq!(halts.check("ITEM"), Ok(()));
        assert!(store.snapshot.lock().unwrap().epoch.is_some());
    }

    #[tokio::test]
    async fn a_halt_applies_at_once_and_reaches_other_gateways_on_their_next_poll() {
        let store = store();
        let (mine, other) = (started(&store).await, started(&store).await);

        mine.halt("ITEM").await.unwrap();
        assert_eq!(mine.check("ITEM"), Err(Refusal::Halted));
        assert_eq!(mine.check("GEM"), Ok(()));
        assert_eq!(other.check("ITEM"), Ok(()), "not polled yet");

        other.refresh().await;
        assert_eq!(other.check("ITEM"), Err(Refusal::Halted));

        other.resume("ITEM").await.unwrap();
        assert_eq!(other.check("ITEM"), Ok(()));
        mine.refresh().await;
        assert_eq!(mine.check("ITEM"), Ok(()));
    }

    #[tokio::test]
    async fn starting_while_the_store_is_down_refuses_every_market() {
        let store = store();
        store.set_down(true);
        let halts = started(&store).await;
        assert_eq!(halts.check("ITEM"), Err(Refusal::Unknown));
        assert!(!halts.is_known());
        assert_eq!(halts.store_healthy(), Some(false));

        store.set_down(false);
        halts.refresh().await;
        assert_eq!(halts.check("ITEM"), Ok(()));
        assert_eq!(halts.store_healthy(), Some(true));
    }

    #[tokio::test]
    async fn a_start_during_an_outage_picks_up_the_halts_it_missed() {
        let store = store();
        started(&store).await.halt("ITEM").await.unwrap();

        store.set_down(true);
        let halts = started(&store).await;
        store.set_down(false);
        halts.refresh().await;
        assert_eq!(halts.check("ITEM"), Err(Refusal::Halted));
    }

    #[tokio::test]
    async fn an_outage_keeps_what_was_last_known() {
        let store = store();
        let halts = started(&store).await;
        halts.halt("ITEM").await.unwrap();

        store.set_down(true);
        halts.refresh().await;
        assert_eq!(halts.check("ITEM"), Err(Refusal::Halted));
        assert_eq!(halts.check("GEM"), Ok(()), "other markets keep trading");
        assert_eq!(halts.store_healthy(), Some(false));
    }

    #[tokio::test]
    async fn a_halt_during_an_outage_says_so_and_lands_when_the_store_returns() {
        let store = store();
        let (mine, other) = (started(&store).await, started(&store).await);

        store.set_down(true);
        assert!(mine.halt("ITEM").await.is_err());
        assert_eq!(mine.check("ITEM"), Err(Refusal::Halted), "in force here regardless");

        store.set_down(false);
        mine.refresh().await;
        assert_eq!(mine.check("ITEM"), Err(Refusal::Halted), "a read of the old list mustn't reopen it");
        assert!(store.holds("ITEM"));
        other.refresh().await;
        assert_eq!(other.check("ITEM"), Err(Refusal::Halted));
    }

    #[tokio::test]
    async fn a_resume_during_an_outage_leaves_the_market_halted() {
        let store = store();
        let halts = started(&store).await;
        halts.halt("ITEM").await.unwrap();

        store.set_down(true);
        assert!(halts.resume("ITEM").await.is_err());
        assert_eq!(halts.check("ITEM"), Err(Refusal::Halted));
        store.set_down(false);
        halts.refresh().await;
        assert_eq!(halts.check("ITEM"), Err(Refusal::Halted));
    }

    #[tokio::test]
    async fn a_store_that_lost_its_data_does_not_reopen_a_market() {
        let store = store();
        let (mine, other) = (started(&store).await, started(&store).await);
        mine.halt("ITEM").await.unwrap();
        other.refresh().await;

        store.lose_everything();
        mine.refresh().await;
        assert_eq!(mine.check("ITEM"), Err(Refusal::Halted));
        other.refresh().await;
        assert_eq!(other.check("ITEM"), Err(Refusal::Halted));

        // The list heals itself, so a gateway that starts now hears about the halt too.
        mine.refresh().await;
        assert!(store.holds("ITEM"));
        let late = started(&store).await;
        assert_eq!(late.check("ITEM"), Err(Refusal::Halted));
    }

    #[tokio::test]
    async fn a_new_gateway_rebuilding_the_list_does_not_clear_an_older_ones_halts() {
        let store = store();
        let old = started(&store).await;
        old.halt("ITEM").await.unwrap();

        store.lose_everything();
        started(&store).await; // Knows nothing, so it claims an empty list with a new epoch.
        old.refresh().await;
        assert_eq!(old.check("ITEM"), Err(Refusal::Halted));
        old.refresh().await;
        assert!(store.holds("ITEM"));
    }

    #[tokio::test]
    async fn a_resume_elsewhere_still_reaches_a_gateway_with_the_same_epoch() {
        let store = store();
        let (mine, other) = (started(&store).await, started(&store).await);
        mine.halt("ITEM").await.unwrap();
        other.refresh().await;
        mine.resume("ITEM").await.unwrap();
        other.refresh().await;
        assert_eq!(other.check("ITEM"), Ok(()));
    }

    /// Holds a read open, so that a change can land while it's in flight.
    struct SlowStore {
        inner: Arc<FakeHaltStore>,
        reading: Notify,
        release: Notify,
        slow: AtomicBool,
    }

    #[async_trait]
    impl HaltStore for SlowStore {
        async fn read(&self) -> Result<Snapshot, String> {
            let snapshot = self.inner.read().await;
            if self.slow.swap(false, Ordering::Relaxed) {
                self.reading.notify_one();
                self.release.notified().await;
            }
            snapshot
        }

        async fn claim_epoch(&self, epoch: &str) -> Result<(), String> {
            self.inner.claim_epoch(epoch).await
        }

        async fn halt(&self, symbol: &str) -> Result<(), String> {
            self.inner.halt(symbol).await
        }

        async fn resume(&self, symbol: &str) -> Result<(), String> {
            self.inner.resume(symbol).await
        }
    }

    #[tokio::test]
    async fn a_read_that_raced_a_halt_cannot_undo_it() {
        let inner = store();
        let slow = Arc::new(SlowStore {
            inner,
            reading: Notify::new(),
            release: Notify::new(),
            slow: AtomicBool::new(false),
        });
        let halts = Arc::new(Halts::shared(slow.clone()));
        halts.refresh().await;

        slow.slow.store(true, Ordering::Relaxed);
        let poll = {
            let halts = halts.clone();
            tokio::spawn(async move { halts.refresh().await })
        };
        slow.reading.notified().await;
        halts.halt("ITEM").await.unwrap();
        slow.release.notify_one();
        poll.await.unwrap();

        assert_eq!(halts.check("ITEM"), Err(Refusal::Halted));
    }

    /// Run by hand against a scratch server, because it deletes the halt list:
    /// `HALTS_TEST_VALKEY_URL=redis://localhost:6390 cargo test -p api real_valkey -- --ignored`
    #[tokio::test]
    #[ignore = "needs a scratch Valkey"]
    async fn real_valkey_round_trip() {
        let url = std::env::var("HALTS_TEST_VALKEY_URL").unwrap();
        let store: Arc<dyn HaltStore> = Arc::new(ValkeyHalts::new(&url).unwrap());
        let client = redis::Client::open(url).unwrap();
        let mut admin = client.get_multiplexed_async_connection().await.unwrap();
        let wipe = || redis::cmd("DEL").arg(HALTS_KEY).clone();
        let _: i64 = wipe().query_async(&mut admin).await.unwrap();

        let (mine, other) = (Halts::shared(store.clone()), Halts::shared(store.clone()));
        mine.refresh().await;
        other.refresh().await;
        assert_eq!(mine.check("ITEM"), Ok(()));
        let epoch = store.read().await.unwrap().epoch.unwrap();
        store.claim_epoch("late").await.unwrap();
        assert_eq!(store.read().await.unwrap().epoch.unwrap(), epoch, "the first claim wins");

        mine.halt("ITEM").await.unwrap();
        other.refresh().await;
        assert_eq!(other.check("ITEM"), Err(Refusal::Halted));

        let _: i64 = wipe().query_async(&mut admin).await.unwrap();
        mine.refresh().await;
        mine.refresh().await;
        assert_eq!(mine.check("ITEM"), Err(Refusal::Halted), "survives losing the hash");
        assert!(store.read().await.unwrap().halted.contains("ITEM"), "and restores it");

        other.resume("ITEM").await.unwrap();
        mine.refresh().await;
        assert_eq!(mine.check("ITEM"), Ok(()));
        let _: i64 = wipe().query_async(&mut admin).await.unwrap();
    }

    #[tokio::test]
    async fn a_dead_valkey_fails_inside_the_timeout() {
        // Port 1 has nothing listening, so this is the real client against a refused connection.
        let halts = Halts::shared(Arc::new(ValkeyHalts::new("redis://127.0.0.1:1").unwrap()));
        let started = std::time::Instant::now();
        halts.refresh().await;
        assert_eq!(halts.check("ITEM"), Err(Refusal::Unknown));
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn without_a_store_halts_work_on_this_gateway() {
        let halts = Halts::standalone();
        assert!(halts.is_known());
        assert_eq!(halts.store_healthy(), None);
        halts.halt("ITEM").await.unwrap();
        assert!(halts.is_halted("ITEM"));
        halts.resume("ITEM").await.unwrap();
        assert!(!halts.is_halted("ITEM"));
    }
}

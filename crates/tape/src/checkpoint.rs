//! Saves a consumer's state together with the tape position it reflects, since a position means
//! nothing without the state built up to it.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use tracing::{error, info};

/// Raise this when a saved state stops meaning what it did, so old files are rebuilt from the tape.
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub struct Checkpoint<T> {
    version: u32,
    pub position: i64,
    /// Whether the consumer had reached the live stream, so restarting a rebuild stays quiet.
    pub caught_up: bool,
    pub state: T,
}

impl<T> Checkpoint<T> {
    pub fn new(position: i64, caught_up: bool, state: T) -> Self {
        Self {
            version: VERSION,
            position,
            caught_up,
            state,
        }
    }
}

pub fn path_for(name: &str) -> PathBuf {
    Path::new(&transport::env_or("CHECKPOINT_DIR", ".run/checkpoints")).join(format!("{name}.json"))
}

/// Replaces the file in one step, so a crash leaves the previous checkpoint intact.
pub fn save<T: Serialize>(path: &Path, checkpoint: &Checkpoint<&T>) -> io::Result<()> {
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)?;
    }
    let temporary = path.with_extension("tmp");
    let mut file = File::create(&temporary)?;
    serde_json::to_writer(&mut file, checkpoint)?;
    file.flush()?;
    file.sync_all()?;
    fs::rename(&temporary, path)
}

/// Reads a checkpoint. A missing or unreadable one means rebuilding from the tape, which is slow
/// but always correct.
pub fn load<T: DeserializeOwned>(path: &Path) -> Option<Checkpoint<T>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            info!("No checkpoint at {}", path.display());
            return None;
        }
        Err(e) => {
            error!("Failed to read {}: {}", path.display(), e);
            return None;
        }
    };
    match serde_json::from_slice::<Checkpoint<T>>(&bytes) {
        Ok(checkpoint) if checkpoint.version == VERSION => Some(checkpoint),
        Ok(checkpoint) => {
            error!(
                "Ignoring {}: version {}",
                path.display(),
                checkpoint.version
            );
            None
        }
        Err(e) => {
            error!("Ignoring {}: {}", path.display(), e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::env;
    use std::process;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT: AtomicU32 = AtomicU32::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            Self(env::temp_dir().join(format!("tape-checkpoint-{}-{id}", process::id())))
        }

        fn file(&self) -> PathBuf {
            self.0.join("nested").join("state.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn round_trips() {
        let dir = TempDir::new();
        let state = vec![1u64, 2, 3];
        save(&dir.file(), &Checkpoint::new(4096, true, &state)).expect("saves");

        let loaded: Checkpoint<Vec<u64>> = load(&dir.file()).expect("loads");
        assert_eq!(loaded.position, 4096);
        assert!(loaded.caught_up);
        assert_eq!(loaded.state, state);
    }

    #[test]
    fn replaces_the_previous_checkpoint() {
        let dir = TempDir::new();
        save(&dir.file(), &Checkpoint::new(1, false, &1u64)).expect("saves");
        save(&dir.file(), &Checkpoint::new(2, false, &2u64)).expect("saves");

        let loaded: Checkpoint<u64> = load(&dir.file()).expect("loads");
        assert_eq!((loaded.position, loaded.state), (2, 2));
        assert!(!dir.file().with_extension("tmp").exists());
    }

    #[test]
    fn a_missing_file_means_no_checkpoint() {
        let dir = TempDir::new();
        assert!(load::<u64>(&dir.file()).is_none());
    }

    #[test]
    fn a_corrupt_file_means_no_checkpoint() {
        let dir = TempDir::new();
        save(&dir.file(), &Checkpoint::new(1, false, &1u64)).expect("saves");
        fs::write(dir.file(), b"{\"version\":1,\"pos").expect("writes");
        assert!(load::<u64>(&dir.file()).is_none());
    }

    #[test]
    fn another_version_means_no_checkpoint() {
        let dir = TempDir::new();
        save(&dir.file(), &Checkpoint::new(1, false, &1u64)).expect("saves");
        fs::write(
            dir.file(),
            b"{\"version\":0,\"position\":1,\"caught_up\":false,\"state\":1}",
        )
        .expect("writes");
        assert!(load::<u64>(&dir.file()).is_none());
    }
}

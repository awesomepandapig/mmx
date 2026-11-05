//! Purges what recovery no longer needs, on its own thread with its own Archive client, because
//! Archive calls block and must not add to the snapshot pause.
//!
//! Each node decides from the snapshots it took itself, so a restarted node purges nothing until
//! it has taken `keep_snapshots` more. Purging never touches matching, so nodes may differ here.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Sender};
use std::thread;

use crate::NodeConfig;
use crate::snapshot::{self, ArchiveClient, Recording};

const LOG_STREAM_ID: i32 = 100;
const CONSENSUS_SNAPSHOT_STREAM_ID: i32 = 107;

/// Never fewer than this, because the newest snapshot isn't in the consensus module's recording
/// log yet when the purge runs. A crash at that moment falls back to the one before it.
const MIN_SNAPSHOTS: usize = 2;

/// Which part of the tape may leave this node's Archive.
#[derive(Debug, Clone, Copy)]
pub struct TapeRetention {
    pub stream_id: i32,
    pub session_id: i32,
    /// The newest bytes to keep, however far consumers have read.
    pub retain_bytes: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct Retention {
    pub keep_snapshots: usize,
    /// With none, the tape is kept forever.
    pub tape: Option<TapeRetention>,
}

/// What a snapshot means for the tape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapeMark {
    /// The output position the snapshot holds. Rebuilding from the tape starts here.
    pub position: i64,
    /// Everything before this has been swept to its long-term home. With none, nothing has.
    pub swept: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Taken {
    recording_id: i64,
    log_position: i64,
    tape: TapeMark,
}

/// The floors below which nothing is needed to recover from any retained snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Floors {
    snapshot_recording_id: i64,
    log_position: i64,
    tape: TapeMark,
}

/// The snapshots this node took and is keeping, oldest first.
struct Ledger {
    keep: usize,
    taken: VecDeque<Taken>,
}

impl Ledger {
    fn new(keep: usize) -> Self {
        let keep = keep.max(MIN_SNAPSHOTS);
        Self { keep, taken: VecDeque::with_capacity(keep + 1) }
    }

    /// Returns floors once enough snapshots exist that the oldest kept one can be relied on.
    fn add(&mut self, taken: Taken) -> Option<Floors> {
        self.taken.push_back(taken);
        if self.taken.len() < self.keep {
            return None;
        }
        self.taken.drain(..self.taken.len() - self.keep);
        let oldest = self.taken.front()?;
        let swept = self.taken.back()?.tape.swept;
        Some(Floors {
            snapshot_recording_id: oldest.recording_id,
            log_position: oldest.log_position,
            tape: TapeMark { position: oldest.tape.position, swept },
        })
    }
}

/// The same calculation as Java's `AeronArchive.segmentFileBasePosition`.
const fn segment_base(start: i64, position: i64, term_length: i64, segment_length: i64) -> i64 {
    let term_base = start - (start & (term_length - 1));
    let from_base = position - term_base;
    term_base + (from_base - (from_base & (segment_length - 1)))
}

/// The new start position that purges up to `floor`, if at least one whole segment is free. Only
/// whole segments can go, so what's kept can start up to one segment before the floor.
fn purge_target(recording: &Recording, floor: i64) -> Option<i64> {
    let term = i64::from(recording.term_length);
    let segment = i64::from(recording.segment_length);
    let target = segment_base(recording.start, floor, term, segment);
    let lowest = segment_base(recording.start, recording.start, term, segment) + segment;
    (target >= lowest).then_some(target)
}

/// The tape may only lose what every limit agrees on. With nothing swept yet, that's nothing.
fn tape_floor(tape: TapeMark, end: i64, retain_bytes: i64) -> Option<i64> {
    Some(tape.swept?.min(tape.position).min(end - retain_bytes))
}

fn purge_old_snapshots(archive: &ArchiveClient, stream_id: i32, below: i64) {
    let recordings = match archive.recordings(stream_id) {
        Ok(recordings) => recordings,
        Err(error) => return eprintln!("failed to list snapshots: {error}"),
    };
    for recording in recordings.iter().filter(|recording| recording.id < below) {
        if let Err(error) = archive.purge_recording(recording.id) {
            eprintln!("failed to purge snapshot recording {}: {error}", recording.id);
        }
    }
}

fn purge_front(archive: &ArchiveClient, name: &str, recording: &Recording, floor: i64) {
    let Some(target) = purge_target(recording, floor) else { return };
    match archive.purge_segments(recording.id, target) {
        Ok(segments) => eprintln!("purged {segments} {name} segments, {name} now starts at {target}"),
        // Most often a replay still reading there. The next snapshot tries again.
        Err(error) => eprintln!("failed to purge the {name} to {target}: {error}"),
    }
}

fn purge(archive: &ArchiveClient, retention: Retention, floors: Floors) {
    purge_old_snapshots(archive, snapshot::SNAPSHOT_STREAM_ID, floors.snapshot_recording_id);
    purge_old_snapshots(archive, CONSENSUS_SNAPSHOT_STREAM_ID, floors.snapshot_recording_id);

    match archive.recordings(LOG_STREAM_ID) {
        Ok(recordings) => {
            if let Some(log) = recordings.iter().max_by_key(|recording| recording.id) {
                purge_front(archive, "log", log, floors.log_position);
            }
        }
        Err(error) => eprintln!("failed to list the log: {error}"),
    }

    let Some(tape) = retention.tape else { return };
    let Some(recording) = archive.find_recording(tape.stream_id, tape.session_id) else { return };
    let end = if recording.stop < 0 { floors.tape.position } else { recording.stop };
    if let Some(floor) = tape_floor(floors.tape, end, tape.retain_bytes) {
        purge_front(archive, "tape", &recording, floor);
    }
}

/// Hands finished snapshots to the purge thread.
pub(crate) struct Housekeeper {
    snapshots: Sender<Taken>,
}

impl Housekeeper {
    pub(crate) fn start(config: &NodeConfig) -> Self {
        let (snapshots, taken) = mpsc::channel::<Taken>();
        let retention = config.retention;
        let config = config.clone();
        thread::Builder::new()
            .name("housekeeper".to_string())
            .spawn(move || {
                let archive = snapshot::connect(&config);
                let mut ledger = Ledger::new(retention.keep_snapshots);
                for snapshot in taken {
                    if let Some(floors) = ledger.add(snapshot) {
                        purge(&archive, retention, floors);
                    }
                }
            })
            .expect("failed to start the housekeeper");
        Self { snapshots }
    }

    pub(crate) fn on_snapshot(&self, recording_id: i64, log_position: i64, tape: TapeMark) {
        // If the thread has died, recovery still works. Disk use just keeps growing.
        let _ = self.snapshots.send(Taken { recording_id, log_position, tape });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEGABYTE: i32 = 1 << 20;

    fn taken(recording_id: i64, swept: Option<i64>) -> Taken {
        Taken {
            recording_id,
            log_position: recording_id * 1_000,
            tape: TapeMark { position: recording_id * 100, swept },
        }
    }

    fn recording(start: i64) -> Recording {
        Recording { id: 1, start, stop: -1, term_length: 16 * MEGABYTE, segment_length: 128 * MEGABYTE }
    }

    #[test]
    fn nothing_is_purged_until_enough_snapshots_exist() {
        let mut ledger = Ledger::new(3);
        assert_eq!(ledger.add(taken(1, None)), None);
        assert_eq!(ledger.add(taken(2, None)), None);
        assert!(ledger.add(taken(3, None)).is_some());
    }

    #[test]
    fn floors_come_from_the_oldest_kept_snapshot() {
        let mut ledger = Ledger::new(2);
        ledger.add(taken(4, None));
        ledger.add(taken(5, None));
        assert_eq!(
            ledger.add(taken(6, Some(70))),
            Some(Floors {
                snapshot_recording_id: 5,
                log_position: 5_000,
                tape: TapeMark { position: 500, swept: Some(70) },
            })
        );
    }

    #[test]
    fn at_least_two_snapshots_are_always_kept() {
        let mut ledger = Ledger::new(1);
        assert_eq!(ledger.add(taken(1, None)), None);
        assert_eq!(ledger.add(taken(2, None)).map(|floors| floors.snapshot_recording_id), Some(1));
    }

    #[test]
    fn the_purge_target_is_a_segment_boundary_at_or_below_the_floor() {
        let segment = i64::from(128 * MEGABYTE);
        assert_eq!(purge_target(&recording(0), 2 * segment + 5), Some(2 * segment));
        assert_eq!(purge_target(&recording(0), 2 * segment), Some(2 * segment));
    }

    #[test]
    fn the_first_segment_can_never_be_purged() {
        let segment = i64::from(128 * MEGABYTE);
        assert_eq!(purge_target(&recording(0), segment - 1), None);
        assert_eq!(purge_target(&recording(segment), 2 * segment - 1), None);
        assert_eq!(purge_target(&recording(segment), 2 * segment), Some(2 * segment));
    }

    #[test]
    fn segments_are_counted_from_the_term_holding_the_start() {
        let (term, segment) = (i64::from(16 * MEGABYTE), i64::from(128 * MEGABYTE));
        let start = term + 4096;
        assert_eq!(purge_target(&recording(start), term + segment + 1), Some(term + segment));
    }

    #[test]
    fn the_tape_stays_until_something_is_swept() {
        assert_eq!(tape_floor(TapeMark { position: 500, swept: None }, 1_000, 0), None);
    }

    #[test]
    fn the_tape_floor_is_the_lowest_limit() {
        let mark = |position, swept| TapeMark { position, swept: Some(swept) };
        assert_eq!(tape_floor(mark(500, 300), 1_000, 100), Some(300));
        assert_eq!(tape_floor(mark(200, 300), 1_000, 100), Some(200));
        assert_eq!(tape_floor(mark(500, 300), 350, 100), Some(250));
    }
}

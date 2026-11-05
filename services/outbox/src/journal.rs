//! The durable copy of the tape.
//!
//! One append-only file holds every frame, in tape order:
//!
//! ```text
//! u32 payload length | u32 crc32 of payload | i64 position | u64 exec_id | u8 flags | frame bytes
//! ```
//!
//! All integers are little endian. Records wait in memory until `sync` writes them and flushes the
//! disk in one step, so several frames share a flush. Readers are limited to the synced length,
//! which is what stops anything undurable from being sent.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Instant;

pub const JOURNAL_FILE: &str = "journal.log";

const RECORD_HEADER: usize = 8;
const PAYLOAD_HEADER: usize = 17;
/// Far longer than any report, so a corrupt length can't cause a huge allocation.
const MAX_PAYLOAD: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The tape position at the end of this frame.
    pub position: i64,
    pub exec_id: u64,
    pub flags: u8,
    pub frame: Vec<u8>,
}

impl Record {
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&[0; RECORD_HEADER]);
        out.extend_from_slice(&self.position.to_le_bytes());
        out.extend_from_slice(&self.exec_id.to_le_bytes());
        out.push(self.flags);
        out.extend_from_slice(&self.frame);

        let payload_length = (out.len() - start - RECORD_HEADER) as u32;
        let crc = crc32fast::hash(&out[start + RECORD_HEADER..]);
        out[start..start + 4].copy_from_slice(&payload_length.to_le_bytes());
        out[start + 4..start + 8].copy_from_slice(&crc.to_le_bytes());
    }

    fn decode(payload: &[u8]) -> Self {
        Self {
            position: i64::from_le_bytes(payload[0..8].try_into().expect("eight bytes")),
            exec_id: u64::from_le_bytes(payload[8..16].try_into().expect("eight bytes")),
            flags: payload[16],
            frame: payload[PAYLOAD_HEADER..].to_vec(),
        }
    }
}

/// Reads one record, or `None` if what's there isn't a whole, valid record.
fn read_record(reader: &mut impl Read) -> io::Result<Option<(Record, u64)>> {
    let mut header = [0u8; RECORD_HEADER];
    if !read_fully(reader, &mut header)? {
        return Ok(None);
    }
    let length = u32::from_le_bytes(header[0..4].try_into().expect("four bytes")) as usize;
    let crc = u32::from_le_bytes(header[4..8].try_into().expect("four bytes"));
    if !(PAYLOAD_HEADER..=MAX_PAYLOAD).contains(&length) {
        return Ok(None);
    }
    let mut payload = vec![0u8; length];
    if !read_fully(reader, &mut payload)? || crc32fast::hash(&payload) != crc {
        return Ok(None);
    }
    Ok(Some((Record::decode(&payload), (RECORD_HEADER + length) as u64)))
}

fn read_fully(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<bool> {
    match reader.read_exact(buffer) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error),
    }
}

/// Flushes a directory, so that a file created or renamed in it survives a power loss.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

pub struct Journal {
    file: File,
    pending: Vec<u8>,
    pending_since: Option<Instant>,
    synced_length: u64,
    position: i64,
    previous_position: i64,
    last_frame: Vec<u8>,
    exec_id: u64,
}

impl Journal {
    /// Opens the journal, handing every surviving record to `visit`. Whatever follows the last
    /// valid record was never reported as durable, so it's cut off.
    pub fn open(dir: &Path, mut visit: impl FnMut(&Record) -> Result<(), String>) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let path = dir.join(JOURNAL_FILE);
        let file = OpenOptions::new().create(true).truncate(false).read(true).append(true).open(&path)?;
        if file.try_lock().is_err() {
            return Err(io::Error::new(ErrorKind::WouldBlock, "another outbox owns this journal"));
        }
        sync_dir(dir)?;

        let mut journal = Self {
            file,
            pending: Vec::new(),
            pending_since: None,
            synced_length: 0,
            position: 0,
            previous_position: 0,
            last_frame: Vec::new(),
            exec_id: 0,
        };

        let total = journal.file.metadata()?.len();
        let mut reader = BufReader::new(File::open(&path)?);
        while let Some((record, length)) = read_record(&mut reader)? {
            visit(&record).map_err(|reason| io::Error::new(ErrorKind::InvalidData, reason))?;
            journal.synced_length += length;
            journal.previous_position = journal.position;
            journal.position = record.position;
            journal.exec_id = record.exec_id;
            journal.last_frame = record.frame;
        }

        if journal.synced_length < total {
            eprintln!("journal: cutting {} bytes after position {}", total - journal.synced_length, journal.position);
            journal.file.set_len(journal.synced_length)?;
        }
        // A record that outlived a crash may never have reached the disk itself.
        journal.file.sync_all()?;
        Ok(journal)
    }

    /// The end of the last frame accepted, durable or not.
    pub fn position(&self) -> i64 {
        self.position
    }

    /// The end of the frame before the last, which is where a reader must start to see the last
    /// frame again.
    pub fn previous_position(&self) -> i64 {
        self.previous_position
    }

    pub fn last_frame(&self) -> &[u8] {
        &self.last_frame
    }

    pub fn is_empty(&self) -> bool {
        self.position == 0
    }

    pub fn exec_id(&self) -> u64 {
        self.exec_id
    }

    pub fn synced_length(&self) -> u64 {
        self.synced_length
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn pending_since(&self) -> Option<Instant> {
        self.pending_since
    }

    pub fn append(&mut self, record: &Record) {
        record.encode_into(&mut self.pending);
        self.pending_since.get_or_insert_with(Instant::now);
        self.previous_position = self.position;
        self.position = record.position;
        self.exec_id = record.exec_id;
        self.last_frame.clear();
        self.last_frame.extend_from_slice(&record.frame);
    }

    /// Writes and flushes everything pending. A failure here must end the process: the kernel may
    /// have dropped the dirty pages, so a retry could report success for data that's gone.
    pub fn sync(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.file.write_all(&self.pending)?;
        self.file.sync_data()?;
        self.synced_length += self.pending.len() as u64;
        self.pending.clear();
        self.pending_since = None;
        Ok(())
    }
}

/// Follows the journal from a byte offset, independently of the writer.
pub struct JournalReader {
    reader: BufReader<File>,
    offset: u64,
}

impl JournalReader {
    pub fn open(dir: &Path, offset: u64) -> io::Result<Self> {
        let mut file = File::open(dir.join(JOURNAL_FILE))?;
        file.seek(SeekFrom::Start(offset))?;
        Ok(Self { reader: BufReader::new(file), offset })
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Reads the next record below `limit`. Everything below it has been flushed, so a bad record
    /// here means the disk has lost data, and that's an error rather than the end of the file.
    pub fn next_below(&mut self, limit: u64) -> io::Result<Option<Record>> {
        if self.offset >= limit {
            return Ok(None);
        }
        match read_record(&mut self.reader)? {
            Some((record, length)) => {
                self.offset += length;
                Ok(Some(record))
            }
            None => Err(io::Error::new(ErrorKind::InvalidData, format!("bad journal record at offset {}", self.offset))),
        }
    }
}

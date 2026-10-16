//! The disk tier of the state cache, laid out for a slow, seek-bound disk.
//!
//! An evaluation is one short-lived process; everything one evaluation reuses
//! from another goes through this directory, which any number of processes
//! share at once. On a disk where every seek costs milliseconds, what an
//! evaluation pays for is the number of places it touches, not the bytes it
//! moves. One file per entry cost an open and a seek for every hit (about
//! 15 ms each on the hosts this serves: a warm evaluation of 2,600 units spent
//! most of a minute seeking), and a file creation and a rename for every write.
//! Here instead:
//!
//! - every process appends what it writes to packs of its own, in the order
//!   it computed it, so writing is sequential and an evaluation that reads the
//!   same units later reads them in the same order. Unit states and rule
//!   contributions go to separate packs: an evaluation that finds every unit
//!   stored reads unit states only, back to back, and contributions are read
//!   only by a unit that missed;
//! - a relation's rows are a blob named by their digest and stored once,
//!   whichever and however many entries hold them (two processes computing
//!   the same rows at the same time may each store them);
//! - one append-only index says which entry is under which key and where each
//!   blob is. A process reads it whole when it first needs it (it is small)
//!   and reads what others appended between strata, so a lookup touches no
//!   file, and the hits of one stratum are read sorted by pack and offset,
//!   with nearby blobs merged into one long read and the kernel asked for
//!   the reads ahead of the readers, so the disk can order them;
//! - eviction, when the store is over its budget, drops whole packs, least
//!   recently used first, and rewrites the index without them. A pack a live
//!   process is still writing is never dropped: its writer holds a shared lock
//!   on it while it lives.
//!
//! ```text
//! <root>/FORMAT              this layout's name and version
//! <root>/index.log           index records, appended by every process
//! <root>/packs/<id>.pack     the blobs one process wrote to one shelf, in order
//! <root>/.maintenance.lock   held by the one process evicting or compacting
//! <root>/.index.lock         shared by appenders, exclusive while the index is replaced
//! ```
//!
//! Nothing is synced. An index record carries a check and a blob its digest,
//! so whatever a crash left short or empty reads as absent, and an absent
//! entry costs one ordinary evaluation.

use crate::cache::{CacheEntry, DiskSweepStats, RelationState};
use crate::rows::Rows;
use parsing::Val;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::warn;

/// The layout this module reads and writes.
const FORMAT: &str = "flowlog-state-store 4\n";
const FORMAT_FILE: &str = "FORMAT";
const INDEX_FILE: &str = "index.log";
const PACKS: &str = "packs";
const LOCK_FILE: &str = ".maintenance.lock";
const INDEX_LOCK_FILE: &str = ".index.lock";

const RECORD_MAGIC: [u8; 4] = *b"FLIX";
const BLOB_MAGIC: [u8; 4] = *b"FLBB";
/// Record header: magic, kind, payload length.
const RECORD_HEADER: usize = 4 + 1 + 4;
/// Record trailer: the check over kind, length and payload.
const RECORD_CHECK: usize = 8;
/// Blob header: magic, payload length, digest.
const BLOB_HEADER: u64 = 4 + 8 + 32;

/// No record is longer; a longer length is damage, not a record to wait for.
const MAX_RECORD: usize = 16 << 20;
/// No blob is longer.
const MAX_BLOB: u64 = 1 << 40;

const KIND_BLOB: u8 = 1;
const KIND_ENTRY: u8 = 2;
const KIND_TOUCH: u8 = 3;

/// Blobs of one pack this close together are read in one request.
const COALESCE_GAP: u64 = 1 << 20;
/// ... as long as the request stays this short.
const MAX_SPAN: u64 = 8 << 20;
/// Threads reading one batch, whatever the evaluation's worker count: they
/// mostly wait for the disk, and requests in flight are the batch's share of
/// a disk other processes are using too.
const READERS: usize = 16;
/// Bytes of a batch the kernel is asked to fetch ahead of its readers, all
/// at once, so that the disk sees them together and can order them.
const READ_AHEAD: u64 = 256 << 20;
/// Bytes a process's pack buffers before it writes them.
const WRITE_BUFFER: usize = 8 << 20;
/// A pack read or written this recently is never evicted: someone may be
/// reading it.
const PROTECT_RECENT: Duration = Duration::from_secs(10 * 60);
/// A pack the index never mentions, this old, was left by a crashed process.
const ORPHAN_AGE: Duration = Duration::from_secs(60 * 60);
/// Eviction stops once the store is back under this share of its budget.
const EVICT_TO: f64 = 0.85;
/// The index is rewritten when it is this many times larger than its live
/// records, and at least `COMPACT_MIN_BYTES`.
const COMPACT_RATIO: u64 = 3;
const COMPACT_MIN_BYTES: u64 = 32 << 20;

/// A key or digest: 32 bytes of SHA-256.
type Name = [u8; 32];

/// Which of a process's packs a blob goes to. A fully cached evaluation reads
/// unit states only; contributions are read when a unit missed, so keeping
/// them apart keeps the unit states of a stratum next to each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shelf {
    Units = 0,
    Contributions = 1,
}

/// A pack's shelf is the low bit of its id.
fn shelf_of(pack: u64) -> Shelf {
    if pack & 1 == 0 { Shelf::Units } else { Shelf::Contributions }
}

/// Where a blob is: its pack, the offset of its header, its payload length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Location {
    pack: u64,
    offset: u64,
    length: u64,
}

impl Location {
    fn end(&self) -> u64 {
        self.offset + BLOB_HEADER + self.length
    }
}

#[derive(Debug, Clone)]
struct StoredRelation {
    name: String,
    digest: Name,
    arity: u32,
    rows: u64,
}

#[derive(Debug, Clone)]
struct StoredEntry {
    relations: Vec<StoredRelation>,
    symbols: Option<Name>,
}

#[derive(Debug, Default, Clone, Copy)]
struct PackInfo {
    /// The end of the furthest blob the index places in the pack.
    bytes: u64,
    /// Milliseconds since the epoch the pack was last read or extended.
    last_used: u64,
}

/// The store as the index describes it.
#[derive(Debug, Default)]
struct Index {
    entries: HashMap<Name, StoredEntry>,
    blobs: HashMap<Name, Location>,
    packs: HashMap<u64, PackInfo>,
    /// Bytes of the index file this process has applied.
    applied: u64,
}

impl Index {
    fn apply(&mut self, kind: u8, payload: &[u8]) {
        match kind {
            KIND_BLOB => {
                if let Some((digest, location)) = decode_blob_record(payload) {
                    let pack = self.packs.entry(location.pack).or_default();
                    pack.bytes = pack.bytes.max(location.end());
                    pack.last_used = pack.last_used.max(pack_created(location.pack));
                    self.blobs.insert(digest, location);
                }
            }
            KIND_ENTRY => {
                if let Some((key, entry)) = decode_entry_record(payload) {
                    self.entries.insert(key, entry);
                }
            }
            KIND_TOUCH => {
                if let Some((time, packs)) = decode_touch_record(payload) {
                    for pack in packs {
                        if let Some(info) = self.packs.get_mut(&pack) {
                            info.last_used = info.last_used.max(time);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// The bytes a rewrite of the index would hold.
    fn compacted_bytes(&self) -> u64 {
        let record = (RECORD_HEADER + RECORD_CHECK) as u64;
        let blobs = self.blobs.len() as u64 * (record + 56);
        let entries: u64 = self
            .entries
            .values()
            .map(|entry| record + 34 + entry.relations.iter().map(|r| 46 + r.name.len() as u64).sum::<u64>())
            .sum();
        blobs + entries + self.packs.len() as u64 * (record + 20)
    }

    fn total_bytes(&self) -> u64 {
        self.packs.values().map(|pack| pack.bytes).sum()
    }
}

/// This process's pack: created on its first write, appended to until exit,
/// and locked shared all that time so that no other process evicts it.
struct PackWriter {
    id: u64,
    file: File,
    buffer: Vec<u8>,
    /// Bytes written to the file so far.
    written: u64,
}

impl PackWriter {
    fn end(&self) -> u64 {
        self.written + self.buffer.len() as u64
    }

    fn drain(&mut self) -> io::Result<()> {
        if !self.buffer.is_empty() {
            // At the offset the index will name, however a failed attempt
            // before this one left the file.
            self.file.write_all_at(&self.buffer, self.written)?;
            self.written += self.buffer.len() as u64;
            self.buffer.clear();
        }
        Ok(())
    }
}

#[derive(Default)]
struct State {
    opened: bool,
    /// The directory cannot be used; the store behaves as an empty one.
    broken: bool,
    index: Index,
    log: Option<File>,
    log_identity: Option<(u64, u64)>,
    /// This process's packs, one per shelf.
    writers: [Option<PackWriter>; 2],
    /// Index records written by this process, not yet appended.
    pending: Vec<u8>,
    /// Packs this process read or relied on and has not yet recorded.
    touched: HashSet<u64>,
    /// Locked shared while appending, exclusive while the index is replaced.
    index_lock: Option<Arc<File>>,
    /// Packs this process has seen on disk.
    present_packs: HashSet<u64>,
    counters: Counters,
}

/// What this process did with the store; reported with the run's counters.
#[derive(Debug, Default, Clone, Copy)]
pub struct Counters {
    pub read_requests: u64,
    pub read_bytes: u64,
    pub written_bytes: u64,
    pub reused_blobs: u64,
}

/// A directory of states shared by every process that points at it.
pub struct DiskStore {
    root: PathBuf,
    max_bytes: u64,
    protect_recent: Duration,
    state: Mutex<State>,
}

impl std::fmt::Debug for DiskStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskStore").field("root", &self.root).field("max_bytes", &self.max_bytes).finish()
    }
}

impl DiskStore {
    pub fn new(root: PathBuf, max_bytes: u64) -> Self {
        Self { root, max_bytes, protect_recent: PROTECT_RECENT, state: Mutex::new(State::default()) }
    }

    /// The same store, evicting packs however recently they were used.
    #[cfg(test)]
    fn unprotected(mut self) -> Self {
        self.protect_recent = Duration::ZERO;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn counters(&self) -> Counters {
        self.state().counters
    }

    /// The entry under each `(key, heads)`, when the store holds one whose
    /// heads are exactly those, every blob of it is intact and its packs are
    /// still there. The blobs of all the hits are read together, sorted by
    /// pack and offset, on up to `threads` threads.
    pub fn load_many(
        &self,
        requests: &[(&str, &BTreeMap<String, usize>)],
        threads: usize,
    ) -> Vec<Option<CacheEntry>> {
        let mut found: Vec<Option<StoredEntry>> = vec![None; requests.len()];
        let mut wanted: BTreeMap<(u64, u64), (Name, Location)> = BTreeMap::new();
        // Open for this call only: a long campaign leaves many packs.
        let mut files: HashMap<u64, File> = HashMap::new();
        {
            let mut state = self.state();
            if !self.prepare(&mut state) {
                return requests.iter().map(|_| None).collect();
            }
            // A blob this process wrote may still be in its buffer.
            for writer in state.writers.iter_mut().flatten() {
                if let Err(error) = writer.drain() {
                    warn!("state store could not write its pack: {error}");
                }
            }
            self.refresh(&mut state);
            for (slot, (key, heads)) in found.iter_mut().zip(requests) {
                let Some(name) = parse_hex(key) else { continue };
                let Some(entry) = state.index.entries.get(&name).cloned() else { continue };
                if !holds_exactly(&entry, heads) {
                    continue;
                }
                let digests = entry.relations.iter().map(|relation| relation.digest).chain(entry.symbols);
                let mut located = Vec::new();
                for digest in digests {
                    match state.index.blobs.get(&digest) {
                        Some(location) => located.push((digest, *location)),
                        None => break,
                    }
                }
                if located.len() != entry.relations.len() + usize::from(entry.symbols.is_some()) {
                    continue;
                }
                let mut available = true;
                for (_, location) in &located {
                    if !files.contains_key(&location.pack) {
                        match File::open(self.pack_path(location.pack)) {
                            Ok(file) => {
                                files.insert(location.pack, file);
                            }
                            Err(_) => available = false,
                        }
                    }
                }
                if !available {
                    continue;
                }
                for (digest, location) in located {
                    wanted.insert((location.pack, location.offset), (digest, location));
                }
                *slot = Some(entry);
            }
        }
        if wanted.is_empty() {
            return requests.iter().map(|_| None).collect();
        }

        // Merge each pack's blobs into a few long reads.
        let mut spans: Vec<(u64, u64, u64, Vec<(Name, Location)>)> = Vec::new();
        for (_, (digest, location)) in wanted {
            match spans.last_mut() {
                Some((pack, start, end, blobs))
                    if *pack == location.pack
                        && location.offset <= *end + COALESCE_GAP
                        && location.end() - *start <= MAX_SPAN =>
                {
                    *end = (*end).max(location.end());
                    blobs.push((digest, location));
                }
                _ => spans.push((location.pack, location.offset, location.end(), vec![(digest, location)])),
            }
        }
        let read_bytes: u64 = spans.iter().map(|(_, start, end, _)| end - start).sum();
        let span_count = spans.len() as u64;

        // Ask for the spans ahead of the readers, a window at a time.
        let ahead_of = |from: usize| {
            let mut bytes = 0;
            spans[from..].iter().take_while(|(_, start, end, _)| {
                bytes += end - start;
                bytes <= READ_AHEAD
            }).count()
        };
        let mut advised = 0;
        let advise = |upto: usize, advised: &mut usize| {
            while *advised < upto.min(spans.len()) {
                let (pack, start, end, _) = &spans[*advised];
                fetch_ahead(&files[pack], *start, end - start);
                *advised += 1;
            }
        };
        advise(ahead_of(0), &mut advised);
        let advised = Mutex::new(advised);
        let order: Vec<usize> = (0..spans.len()).collect();

        // Read, check and decode every blob; a damaged one is simply absent.
        let decoded: Vec<Vec<(Name, Option<Vec<u8>>)>> = parallel(order, threads.max(READERS), |index| {
            {
                let mut advised = advised.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                advise(index + ahead_of(index), &mut advised);
            }
            let (pack, start, end, blobs) = &spans[index];
            let (pack, start, end) = (*pack, *start, *end);
            let blobs = blobs.clone();
            let file = &files[&pack];
            let read = |start: u64, length: u64| {
                let mut bytes = vec![0u8; length as usize];
                file.read_exact_at(&mut bytes, start).ok().map(|()| bytes)
            };
            let Some(bytes) = read(start, end - start) else {
                // A pack cut short by a crash fails the long read; the blobs
                // before the cut are still there, one by one.
                return blobs
                    .into_iter()
                    .map(|(digest, location)| {
                        let blob = read(location.offset, BLOB_HEADER + location.length)
                            .and_then(|bytes| verified_payload(&bytes, &digest, location.length));
                        (digest, blob)
                    })
                    .collect();
            };
            blobs
                .into_iter()
                .map(|(digest, location)| {
                    let at = (location.offset - start) as usize;
                    let blob = bytes.get(at..at + (BLOB_HEADER + location.length) as usize)
                        .and_then(|bytes| verified_payload(bytes, &digest, location.length));
                    (digest, blob)
                })
                .collect()
        });
        let mut blobs: HashMap<Name, Vec<u8>> = HashMap::new();
        let mut damaged: Vec<Name> = Vec::new();
        for (digest, blob) in decoded.into_iter().flatten() {
            match blob {
                Some(blob) => {
                    blobs.insert(digest, blob);
                }
                None => damaged.push(digest),
            }
        }

        let mut used_packs: HashSet<u64> = HashSet::new();
        let mut entries = Vec::with_capacity(requests.len());
        let mut states: HashMap<Name, Arc<RelationState>> = HashMap::new();
        for stored in found {
            let entry = stored.and_then(|stored| {
                let mut relations = BTreeMap::new();
                for relation in &stored.relations {
                    let state = match states.get(&relation.digest) {
                        Some(state) if state.name == relation.name => Arc::clone(state),
                        _ => {
                            let rows = decode_rows(blobs.get(&relation.digest)?)?;
                            if rows.arity() != relation.arity as usize || rows.len() as u64 != relation.rows {
                                return None;
                            }
                            let state = Arc::new(RelationState::with_digest(
                                relation.name.clone(), rows.arity(), rows, relation.digest,
                            ));
                            states.insert(relation.digest, Arc::clone(&state));
                            state
                        }
                    };
                    relations.insert(relation.name.clone(), state);
                }
                let symbols = match stored.symbols {
                    Some(digest) => decode_symbols(blobs.get(&digest)?)?,
                    None => Vec::new(),
                };
                Some(CacheEntry::with_symbols(relations, symbols))
            });
            entries.push(entry);
        }
        {
            let mut state = self.state();
            state.counters.read_requests += span_count;
            state.counters.read_bytes += read_bytes;
            for digest in &damaged {
                // Whatever is there is not this blob; forget where it was.
                state.index.blobs.remove(digest);
            }
            for (digest, _) in blobs.iter() {
                if let Some(location) = state.index.blobs.get(digest) {
                    used_packs.insert(location.pack);
                }
            }
            let now = now_millis();
            for pack in used_packs {
                if let Some(info) = state.index.packs.get_mut(&pack) {
                    info.last_used = info.last_used.max(now);
                }
                state.touched.insert(pack);
            }
        }
        entries
    }

    /// Record a unit's states, or one rule's contribution, under `key`. Rows
    /// already in the store are not written again; the rest go to this
    /// process's pack for `shelf`. Nothing reaches the disk until `flush`.
    pub fn put(&self, key: &str, entry: &CacheEntry, shelf: Shelf) {
        let Some(name) = parse_hex(key) else { return };
        let mut state = self.state();
        if !self.prepare(&mut state) {
            return;
        }
        let mut relations = Vec::with_capacity(entry.relations.len());
        for (relation_name, relation) in &entry.relations {
            if self.place(&mut state, &relation.digest, shelf, |out| encode_rows(&relation.rows, out)).is_none() {
                return;
            }
            relations.push(StoredRelation {
                name: relation_name.clone(),
                digest: relation.digest,
                arity: relation.arity as u32,
                rows: relation.rows.len() as u64,
            });
        }
        let symbols = if entry.symbols.is_empty() {
            None
        } else {
            let mut payload = Vec::new();
            encode_symbols(&entry.symbols, &mut payload);
            let digest: Name = Sha256::digest(&payload).into();
            if self.place(&mut state, &digest, shelf, |out| out.extend_from_slice(&payload)).is_none() {
                return;
            }
            Some(digest)
        };
        let stored = StoredEntry { relations, symbols };
        let mut payload = Vec::new();
        encode_entry_record(&name, &stored, &mut payload);
        push_record(&mut state.pending, KIND_ENTRY, &payload);
        state.index.entries.insert(name, stored);
    }

    /// The blob under `digest`, in the store already or appended now. A unit
    /// state stored only among contributions is written again with the units,
    /// so that reading units never visits a contributions pack.
    fn place(&self, state: &mut State, digest: &Name, shelf: Shelf, encode: impl FnOnce(&mut Vec<u8>)) -> Option<()> {
        if let Some(location) = state.index.blobs.get(digest).copied() {
            let own = state.writers.iter().flatten().any(|writer| writer.id == location.pack);
            let present = own
                || state.present_packs.contains(&location.pack)
                || (self.pack_path(location.pack).exists() && state.present_packs.insert(location.pack));
            let shelved = shelf == Shelf::Contributions || shelf_of(location.pack) == Shelf::Units;
            if present && shelved {
                // This unit now relies on that pack too.
                state.touched.insert(location.pack);
                if let Some(info) = state.index.packs.get_mut(&location.pack) {
                    info.last_used = info.last_used.max(now_millis());
                }
                state.counters.reused_blobs += 1;
                return Some(());
            }
        }
        let slot = shelf as usize;
        if state.writers[slot].is_none() {
            match self.create_pack(shelf) {
                Ok(writer) => state.writers[slot] = Some(writer),
                Err(error) => {
                    warn!("state store at {} cannot create a pack: {error}", self.root.display());
                    state.broken = true;
                    return None;
                }
            }
        }
        let writer = state.writers[slot].as_mut().expect("a pack was just created");
        let offset = writer.end();
        writer.buffer.extend_from_slice(&BLOB_MAGIC);
        writer.buffer.extend_from_slice(&0u64.to_le_bytes());
        writer.buffer.extend_from_slice(digest);
        let payload_start = writer.buffer.len();
        encode(&mut writer.buffer);
        let length = (writer.buffer.len() - payload_start) as u64;
        let length_at = payload_start - 32 - 8;
        writer.buffer[length_at..length_at + 8].copy_from_slice(&length.to_le_bytes());
        let location = Location { pack: writer.id, offset, length };
        let pack = writer.id;
        if writer.buffer.len() >= WRITE_BUFFER {
            if let Err(error) = writer.drain() {
                warn!("state store could not write its pack: {error}");
                state.broken = true;
                return None;
            }
        }
        state.counters.written_bytes += BLOB_HEADER + length;
        let mut payload = Vec::with_capacity(56);
        encode_blob_record(digest, &location, &mut payload);
        push_record(&mut state.pending, KIND_BLOB, &payload);
        let info = state.index.packs.entry(pack).or_default();
        info.bytes = info.bytes.max(location.end());
        info.last_used = now_millis();
        state.index.blobs.insert(*digest, location);
        Some(())
    }

    /// Write this process's buffered blobs, then the index records that name
    /// them, so that no record ever points past what was written.
    pub fn flush(&self) {
        let mut state = self.state();
        if !state.opened || state.broken {
            return;
        }
        for writer in state.writers.iter_mut().flatten() {
            if let Err(error) = writer.drain() {
                warn!("state store could not write its pack: {error}");
                state.broken = true;
                return;
            }
        }
        if !state.touched.is_empty() {
            let packs: Vec<u64> = state.touched.drain().collect();
            let mut payload = Vec::with_capacity(12 + 8 * packs.len());
            encode_touch_record(now_millis(), &packs, &mut payload);
            push_record(&mut state.pending, KIND_TOUCH, &payload);
        }
        if state.pending.is_empty() {
            return;
        }
        // While this lock is held the index is not replaced, so the records
        // land in the file every process reads, not in one being retired.
        let _locked = state.index_lock.clone().and_then(|lock| Locked::shared(lock).ok());
        self.follow_replacement(&mut state);
        let pending = std::mem::take(&mut state.pending);
        let Some(log) = state.log.as_mut() else { return };
        // One write: appends of concurrent processes do not interleave.
        if let Err(error) = log.write_all(&pending) {
            warn!("state store could not append to its index: {error}");
        }
    }

    /// Bring the store back under its budget when this run left it over:
    /// drop the least recently used packs and rewrite the index without them.
    /// Only one process does this at a time; the others carry on.
    pub fn maintain(&self) -> DiskSweepStats {
        let mut stats = DiskSweepStats::default();
        let mut state = self.state();
        if !self.prepare(&mut state) {
            return stats;
        }
        let log_size = state.log.as_ref().and_then(|log| log.metadata().ok()).map_or(0, |m| m.len());
        let over_budget = self.max_bytes > 0 && state.index.total_bytes() + log_size > self.max_bytes;
        let bloated = log_size >= COMPACT_MIN_BYTES
            && log_size > COMPACT_RATIO * state.index.compacted_bytes().max(1);
        if !over_budget && !bloated {
            return stats;
        }
        let started = Instant::now();
        let lock = match OpenOptions::new().create(true).truncate(false).write(true).open(self.root.join(LOCK_FILE)) {
            Ok(lock) => lock,
            Err(error) => {
                warn!("state store maintenance at {}: {error}", self.root.display());
                return stats;
            }
        };
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                stats.disk_sweep_skips += 1;
                return stats;
            }
            Err(fs::TryLockError::Error(error)) => {
                warn!("state store maintenance at {}: {error}", self.root.display());
                return stats;
            }
        }
        self.refresh(&mut state);
        if let Err(error) = self.evict_and_compact(&mut state, &mut stats) {
            warn!("state store maintenance at {}: {error}", self.root.display());
        }
        stats.disk_sweeps += 1;
        stats.disk_sweep_micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        stats
    }

    fn evict_and_compact(&self, state: &mut State, stats: &mut DiskSweepStats) -> io::Result<()> {
        let now = now_millis();
        let own: Vec<u64> = state.writers.iter().flatten().map(|writer| writer.id).collect();
        // What is on disk: every pack, and when it was last written.
        let mut on_disk: Vec<(u64, u64, u64)> = Vec::new();
        for item in fs::read_dir(self.root.join(PACKS))? {
            let item = item?;
            let Some(id) = item.file_name().to_str().and_then(parse_pack_name) else { continue };
            let metadata = item.metadata()?;
            let modified = metadata.modified().ok().and_then(|time| time.duration_since(UNIX_EPOCH).ok());
            on_disk.push((id, metadata.len(), modified.map_or(0, |time| time.as_millis() as u64)));
        }
        stats.disk_files_examined += on_disk.len();
        let mut removed: HashSet<u64> = HashSet::new();
        // A pack no record names, written long ago, belonged to a crash.
        for &(id, size, modified) in &on_disk {
            if !state.index.packs.contains_key(&id)
                && !own.contains(&id)
                && now.saturating_sub(modified) > ORPHAN_AGE.as_millis() as u64
                && self.remove_pack(id)
            {
                removed.insert(id);
                stats.disk_files_removed += 1;
                stats.disk_bytes_removed += size;
            }
        }
        let mut total: u64 = on_disk.iter().filter(|(id, _, _)| !removed.contains(id)).map(|(_, size, _)| size).sum();
        let target = (self.max_bytes as f64 * EVICT_TO) as u64;
        if self.max_bytes > 0 && total > self.max_bytes {
            let mut candidates: Vec<(u64, u64, u64)> = on_disk
                .iter()
                .filter(|(id, _, _)| !removed.contains(id) && !own.contains(id))
                .map(|&(id, size, modified)| {
                    let used = state.index.packs.get(&id).map_or(modified, |info| info.last_used.max(modified));
                    (used, id, size)
                })
                .collect();
            candidates.sort_unstable();
            for (used, id, size) in candidates {
                if total <= target || now.saturating_sub(used) < self.protect_recent.as_millis() as u64 {
                    break;
                }
                if self.remove_pack(id) {
                    removed.insert(id);
                    total = total.saturating_sub(size);
                    stats.disk_files_removed += 1;
                    stats.disk_bytes_removed += size;
                }
            }
        }
        let log_size = state.log.as_ref().and_then(|log| log.metadata().ok()).map_or(0, |m| m.len());
        let bloated = log_size >= COMPACT_MIN_BYTES
            && log_size > COMPACT_RATIO * state.index.compacted_bytes().max(1);
        let present: HashSet<u64> = on_disk.iter().map(|(id, _, _)| *id).filter(|id| !removed.contains(id)).collect();
        // Packs the index names that are gone: it would count them forever.
        let missing = state.index.packs.keys().any(|id| !present.contains(id) && !own.contains(id));
        if removed.is_empty() && !bloated && !missing {
            return Ok(());
        }
        self.rewrite_index(state, &present)
    }

    /// Remove a pack, unless a live process still holds it to write it.
    fn remove_pack(&self, id: u64) -> bool {
        let path = self.pack_path(id);
        let Ok(pack) = File::open(&path) else { return false };
        match pack.try_lock() {
            Ok(()) => fs::remove_file(&path).is_ok(),
            Err(fs::TryLockError::WouldBlock) => false,
            // Where locks are not supported, age alone protects it.
            Err(fs::TryLockError::Error(_)) => fs::remove_file(&path).is_ok(),
        }
    }

    /// Replace the index with a file of only the records it still needs: of
    /// packs in `present`, this process's, or ones the index learned of since
    /// `present` was listed. Appenders wait while the last records of the old
    /// file are carried over and the new file takes its place, so none is lost.
    fn rewrite_index(&self, state: &mut State, present: &HashSet<u64>) -> io::Result<()> {
        for writer in state.writers.iter_mut().flatten() {
            writer.drain()?;
        }
        let lock = state.index_lock.clone().ok_or_else(|| io::Error::other("the index lock is not open"))?;
        let _locked = Locked::exclusive(lock)?;
        let known: HashSet<u64> = state.index.packs.keys().copied().collect();
        self.read_index(state);
        let own: Vec<u64> = state.writers.iter().flatten().map(|writer| writer.id).collect();
        let index = &mut state.index;
        index.packs.retain(|id, _| present.contains(id) || own.contains(id) || !known.contains(id));
        let packs = &index.packs;
        index.blobs.retain(|_, location| packs.contains_key(&location.pack));
        let blobs = &index.blobs;
        index.entries.retain(|_, entry| {
            entry.relations.iter().all(|relation| blobs.contains_key(&relation.digest))
                && entry.symbols.is_none_or(|digest| blobs.contains_key(&digest))
        });
        let mut out = Vec::with_capacity(state.index.compacted_bytes() as usize);
        let mut payload = Vec::new();
        for (digest, location) in &state.index.blobs {
            payload.clear();
            encode_blob_record(digest, location, &mut payload);
            push_record(&mut out, KIND_BLOB, &payload);
        }
        for (key, entry) in &state.index.entries {
            payload.clear();
            encode_entry_record(key, entry, &mut payload);
            push_record(&mut out, KIND_ENTRY, &payload);
        }
        let mut by_time: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for (id, info) in &state.index.packs {
            by_time.entry(info.last_used).or_default().push(*id);
        }
        for (time, packs) in by_time {
            payload.clear();
            encode_touch_record(time, &packs, &mut payload);
            push_record(&mut out, KIND_TOUCH, &payload);
        }
        let temporary = self.root.join(format!(".{INDEX_FILE}.{}.{}", std::process::id(), now_millis()));
        let result = (|| {
            fs::write(&temporary, &out)?;
            fs::rename(&temporary, self.root.join(INDEX_FILE))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
            return result;
        }
        // What this process had still to append is in the new file.
        state.pending.clear();
        let log = OpenOptions::new().read(true).append(true).open(self.root.join(INDEX_FILE))?;
        let metadata = log.metadata()?;
        state.log_identity = Some((metadata.dev(), metadata.ino()));
        state.index.applied = out.len() as u64;
        state.log = Some(log);
        Ok(())
    }

    /// The directory ready and the index read, once per process. Any failure
    /// leaves the store empty for this process rather than failing the run.
    fn prepare(&self, state: &mut State) -> bool {
        if state.opened {
            return !state.broken;
        }
        state.opened = true;
        if let Err(error) = self.open(state) {
            warn!("state store at {} is unusable: {error}", self.root.display());
            state.broken = true;
        }
        !state.broken
    }

    fn open(&self, state: &mut State) -> io::Result<()> {
        fs::create_dir_all(self.root.join(PACKS))?;
        let format = self.root.join(FORMAT_FILE);
        let name_layout = || {
            let temporary = self.root.join(format!(".{FORMAT_FILE}.{}", std::process::id()));
            fs::write(&temporary, FORMAT)?;
            fs::rename(&temporary, &format)
        };
        match fs::read_to_string(&format) {
            Ok(text) if text == FORMAT => {}
            // Empty or short: a crash came before the name reached the disk.
            Ok(text) if FORMAT.starts_with(text.as_str()) => name_layout()?,
            Ok(text) => {
                return Err(io::Error::other(format!(
                    "{} names another layout ({:?})",
                    format.display(),
                    text.trim()
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => name_layout()?,
            Err(error) => return Err(error),
        }
        let lock = OpenOptions::new().create(true).truncate(false).write(true).open(self.root.join(INDEX_LOCK_FILE))?;
        state.index_lock = Some(Arc::new(lock));
        let log = OpenOptions::new().read(true).append(true).create(true).open(self.root.join(INDEX_FILE))?;
        let metadata = log.metadata()?;
        state.log_identity = Some((metadata.dev(), metadata.ino()));
        state.log = Some(log);
        state.index = Index::default();
        self.refresh(state);
        Ok(())
    }

    /// Reopen the index if another process replaced it while compacting.
    fn follow_replacement(&self, state: &mut State) {
        let Ok(current) = fs::metadata(self.root.join(INDEX_FILE)) else { return };
        if state.log_identity == Some((current.dev(), current.ino())) {
            return;
        }
        let reopened = OpenOptions::new().read(true).append(true).open(self.root.join(INDEX_FILE));
        match reopened.and_then(|log| log.metadata().map(|metadata| (log, metadata))) {
            Ok((log, metadata)) => {
                state.log_identity = Some((metadata.dev(), metadata.ino()));
                state.log = Some(log);
                // The new file restates everything appended to the old one
                // that is still live; read it from the start, then what this
                // process has still to append.
                state.index = Index::default();
                self.read_index(state);
                let pending = std::mem::take(&mut state.pending);
                parse_records(&pending, |kind, payload| state.index.apply(kind, payload));
                state.pending = pending;
            }
            Err(error) => warn!("state store could not reopen its index: {error}"),
        }
    }

    /// Apply what other processes appended since this one last looked.
    fn refresh(&self, state: &mut State) {
        if state.broken {
            return;
        }
        self.follow_replacement(state);
        self.read_index(state);
    }

    fn read_index(&self, state: &mut State) {
        let Some(log) = state.log.as_ref() else { return };
        let Ok(size) = log.metadata().map(|metadata| metadata.len()) else { return };
        let from = state.index.applied;
        if size <= from {
            return;
        }
        let mut bytes = vec![0u8; (size - from) as usize];
        if log.read_exact_at(&mut bytes, from).is_err() {
            return;
        }
        let consumed = parse_records(&bytes, |kind, payload| state.index.apply(kind, payload));
        state.index.applied = from + consumed as u64;
    }

    fn create_pack(&self, shelf: Shelf) -> io::Result<PackWriter> {
        let mut attempt = 0u64;
        loop {
            let unique = (u64::from(std::process::id()).wrapping_mul(2654435761) + 2 * attempt) & 0xfffe;
            let id = (now_millis() << 16) | unique | shelf as u64;
            match OpenOptions::new().write(true).create_new(true).open(self.pack_path(id)) {
                Ok(file) => {
                    // Where locks are not supported, age alone protects it.
                    let _ = file.lock_shared();
                    return Ok(PackWriter { id, file, buffer: Vec::with_capacity(WRITE_BUFFER), written: 0 });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists && attempt < 1024 => attempt += 1,
                Err(error) => return Err(error),
            }
        }
    }

    fn pack_path(&self, id: u64) -> PathBuf {
        self.root.join(PACKS).join(format!("{id:016x}.pack"))
    }
}

/// Ask the kernel to start reading a range of `file` into its cache, without
/// waiting for it: a hint, which costs nothing when ignored.
fn fetch_ahead(file: &File, offset: u64, length: u64) {
    use std::os::fd::AsRawFd;
    let (Ok(offset), Ok(length)) = (libc::off_t::try_from(offset), libc::off_t::try_from(length)) else {
        return;
    };
    // SAFETY: an advisory call on a descriptor this process holds open; it
    // touches no memory of this process.
    unsafe {
        libc::posix_fadvise(file.as_raw_fd(), offset, length, libc::POSIX_FADV_WILLNEED);
    }
}

/// Whether `entry` holds exactly the relations `heads` names, each once and
/// of the arity named.
fn holds_exactly(entry: &StoredEntry, heads: &BTreeMap<String, usize>) -> bool {
    let mut seen = HashSet::with_capacity(entry.relations.len());
    entry.relations.len() == heads.len()
        && entry.relations.iter().all(|relation| {
            seen.insert(relation.name.as_str())
                && heads.get(&relation.name).is_some_and(|arity| *arity == relation.arity as usize)
        })
}

/// A lock on a file, released when dropped.
struct Locked(Arc<File>);

impl Locked {
    fn shared(file: Arc<File>) -> io::Result<Self> {
        file.lock_shared()?;
        Ok(Self(file))
    }

    fn exclusive(file: Arc<File>) -> io::Result<Self> {
        file.lock()?;
        Ok(Self(file))
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Check a blob read from its pack against the digest the index names it
/// by, and hand back its payload.
fn verified_payload(bytes: &[u8], digest: &Name, length: u64) -> Option<Vec<u8>> {
    if bytes.len() as u64 != BLOB_HEADER + length || bytes[..4] != BLOB_MAGIC {
        return None;
    }
    if u64::from_le_bytes(bytes[4..12].try_into().ok()?) != length || &bytes[12..44] != digest {
        return None;
    }
    let payload = &bytes[44..];
    if <[u8; 32]>::from(Sha256::digest(payload)) != *digest {
        return None;
    }
    Some(payload.to_vec())
}

/// A row blob: its arity, its row count, then every cell little-endian. The
/// SHA-256 of these bytes is the relation's digest, so a blob of rows is
/// named by the same digest the state already carries.
fn encode_rows(rows: &Rows, out: &mut Vec<u8>) {
    out.extend_from_slice(&(rows.arity() as u64).to_le_bytes());
    out.extend_from_slice(&(rows.len() as u64).to_le_bytes());
    #[cfg(target_endian = "little")]
    {
        // SAFETY: `Val` is a plain integer; on a little-endian target its
        // memory is its little-endian encoding.
        let cells = rows.cells();
        out.extend_from_slice(unsafe {
            std::slice::from_raw_parts(cells.as_ptr().cast::<u8>(), std::mem::size_of_val(cells))
        });
    }
    #[cfg(not(target_endian = "little"))]
    for cell in rows.cells() {
        out.extend_from_slice(&cell.to_le_bytes());
    }
}

fn decode_rows(payload: &[u8]) -> Option<Rows> {
    let rest = payload;
    if rest.len() < 16 {
        return None;
    }
    let arity = usize::try_from(u64::from_le_bytes(rest[..8].try_into().ok()?)).ok()?;
    let rows = usize::try_from(u64::from_le_bytes(rest[8..16].try_into().ok()?)).ok()?;
    let cells = &rest[16..];
    if cells.len() != arity.checked_mul(rows)?.checked_mul(8)? || (arity == 0 && rows > 1) {
        return None;
    }
    let values = cells.chunks_exact(8).map(|cell| Val::from_le_bytes(cell.try_into().expect("8 bytes"))).collect();
    Some(Rows::from_cells(arity, values, rows))
}

fn encode_symbols(symbols: &[(Val, String)], out: &mut Vec<u8>) {
    out.extend_from_slice(b"S");
    out.extend_from_slice(&(symbols.len() as u64).to_le_bytes());
    for (id, text) in symbols {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
    }
}

fn decode_symbols(payload: &[u8]) -> Option<Vec<(Val, String)>> {
    let (&tag, mut rest) = payload.split_first()?;
    if tag != b'S' || rest.len() < 8 {
        return None;
    }
    let count = u64::from_le_bytes(rest[..8].try_into().ok()?);
    rest = &rest[8..];
    let mut symbols = Vec::new();
    for _ in 0..count {
        if rest.len() < 12 {
            return None;
        }
        let id = Val::from_le_bytes(rest[..8].try_into().ok()?);
        let length = u32::from_le_bytes(rest[8..12].try_into().ok()?) as usize;
        let text = std::str::from_utf8(rest.get(12..12 + length)?).ok()?.to_string();
        symbols.push((id, text));
        rest = &rest[12 + length..];
    }
    rest.is_empty().then_some(symbols)
}

fn record_check(kind: u8, payload: &[u8]) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update([kind]);
    hasher.update((payload.len() as u32).to_le_bytes());
    hasher.update(payload);
    let digest: [u8; 32] = hasher.finalize().into();
    u64::from_le_bytes(digest[..8].try_into().expect("8 bytes"))
}

fn push_record(out: &mut Vec<u8>, kind: u8, payload: &[u8]) {
    out.extend_from_slice(&RECORD_MAGIC);
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out.extend_from_slice(&record_check(kind, payload).to_le_bytes());
}

/// Apply every intact record of `bytes`; return how many bytes were settled.
///
/// A record cut short with nothing intact after it is an append still being
/// written: it is left for the next read. One cut short or damaged with
/// intact records after it was left by a crash: it is skipped, and what it
/// said is lost, not wrong.
fn parse_records(bytes: &[u8], mut apply: impl FnMut(u8, &[u8])) -> usize {
    let mut at = 0;
    while at < bytes.len() {
        match record_at(bytes, at) {
            Some(Ok((kind, payload, end))) => {
                apply(kind, payload);
                at = end;
            }
            Some(Err(())) => match next_record(bytes, at + 1) {
                Some(next) => at = next,
                None => return at + skippable(&bytes[at..]),
            },
            None => match next_record(bytes, at + 1) {
                Some(next) => at = next,
                None => break,
            },
        }
    }
    at
}

/// The record starting at `at`: `None` when it does not fit in `bytes` yet,
/// `Some(Err)` when it is not a record at all.
#[allow(clippy::type_complexity)]
fn record_at(bytes: &[u8], at: usize) -> Option<Result<(u8, &[u8], usize), ()>> {
    let header = bytes.get(at..at + RECORD_HEADER)?;
    if header[..4] != RECORD_MAGIC {
        return Some(Err(()));
    }
    let kind = header[4];
    let length = u32::from_le_bytes(header[5..9].try_into().expect("4 bytes")) as usize;
    if length > MAX_RECORD {
        return Some(Err(()));
    }
    let end = at + RECORD_HEADER + length + RECORD_CHECK;
    let record = bytes.get(at..end)?;
    let payload = &record[RECORD_HEADER..RECORD_HEADER + length];
    let check = u64::from_le_bytes(record[RECORD_HEADER + length..].try_into().expect("8 bytes"));
    if check == record_check(kind, payload) {
        Some(Ok((kind, payload, end)))
    } else {
        Some(Err(()))
    }
}

/// The start of the first intact record at or after `from`.
fn next_record(bytes: &[u8], from: usize) -> Option<usize> {
    let mut at = from;
    while at < bytes.len() {
        let candidate = at + find_magic(&bytes[at..])?;
        if let Some(Ok(_)) = record_at(bytes, candidate) {
            return Some(candidate);
        }
        at = candidate + 1;
    }
    None
}

/// Bytes of damage that can be passed over for good: all but a tail that
/// might still become the start of a record.
fn skippable(bytes: &[u8]) -> usize {
    match bytes.iter().skip(1).position(|&byte| byte == RECORD_MAGIC[0]) {
        Some(position) => position + 1,
        None => bytes.len(),
    }
}

fn find_magic(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == RECORD_MAGIC)
}

fn encode_blob_record(digest: &Name, location: &Location, out: &mut Vec<u8>) {
    out.extend_from_slice(digest);
    out.extend_from_slice(&location.pack.to_le_bytes());
    out.extend_from_slice(&location.offset.to_le_bytes());
    out.extend_from_slice(&location.length.to_le_bytes());
}

fn decode_blob_record(payload: &[u8]) -> Option<(Name, Location)> {
    if payload.len() != 56 {
        return None;
    }
    let digest: Name = payload[..32].try_into().ok()?;
    let word = |at: usize| u64::from_le_bytes(payload[at..at + 8].try_into().expect("8 bytes"));
    let location = Location { pack: word(32), offset: word(40), length: word(48) };
    // A location no pack could hold is damage, whatever its check says.
    location.offset.checked_add(BLOB_HEADER)?.checked_add(location.length)?;
    (location.length <= MAX_BLOB).then_some((digest, location))
}

fn encode_entry_record(key: &Name, entry: &StoredEntry, out: &mut Vec<u8>) {
    out.extend_from_slice(key);
    out.extend_from_slice(&(entry.relations.len() as u16).to_le_bytes());
    for relation in &entry.relations {
        out.extend_from_slice(&(relation.name.len() as u16).to_le_bytes());
        out.extend_from_slice(relation.name.as_bytes());
        out.extend_from_slice(&relation.digest);
        out.extend_from_slice(&relation.arity.to_le_bytes());
        out.extend_from_slice(&relation.rows.to_le_bytes());
    }
    match &entry.symbols {
        Some(digest) => {
            out.push(1);
            out.extend_from_slice(digest);
        }
        None => out.push(0),
    }
}

fn decode_entry_record(payload: &[u8]) -> Option<(Name, StoredEntry)> {
    let key: Name = payload.get(..32)?.try_into().ok()?;
    let mut rest = payload.get(32..)?;
    let count = u16::from_le_bytes(rest.get(..2)?.try_into().ok()?) as usize;
    rest = &rest[2..];
    let mut relations = Vec::with_capacity(count);
    for _ in 0..count {
        let length = u16::from_le_bytes(rest.get(..2)?.try_into().ok()?) as usize;
        let name = std::str::from_utf8(rest.get(2..2 + length)?).ok()?.to_string();
        rest = &rest[2 + length..];
        let digest: Name = rest.get(..32)?.try_into().ok()?;
        let arity = u32::from_le_bytes(rest.get(32..36)?.try_into().ok()?);
        let rows = u64::from_le_bytes(rest.get(36..44)?.try_into().ok()?);
        rest = &rest[44..];
        relations.push(StoredRelation { name, digest, arity, rows });
    }
    let symbols = match rest.split_first()? {
        (0, []) => None,
        (1, digest) if digest.len() == 32 => Some(digest.try_into().ok()?),
        _ => return None,
    };
    Some((key, StoredEntry { relations, symbols }))
}

fn encode_touch_record(time: u64, packs: &[u64], out: &mut Vec<u8>) {
    out.extend_from_slice(&time.to_le_bytes());
    out.extend_from_slice(&(packs.len() as u32).to_le_bytes());
    for pack in packs {
        out.extend_from_slice(&pack.to_le_bytes());
    }
}

fn decode_touch_record(payload: &[u8]) -> Option<(u64, Vec<u64>)> {
    let time = u64::from_le_bytes(payload.get(..8)?.try_into().ok()?);
    let count = u32::from_le_bytes(payload.get(8..12)?.try_into().ok()?) as usize;
    let packs = payload.get(12..)?;
    if packs.len() != count * 8 {
        return None;
    }
    Some((time, packs.chunks_exact(8).map(|pack| u64::from_le_bytes(pack.try_into().expect("8 bytes"))).collect()))
}

fn parse_hex(key: &str) -> Option<Name> {
    if key.len() != 64 {
        return None;
    }
    let mut name = [0u8; 32];
    for (index, pair) in key.as_bytes().chunks_exact(2).enumerate() {
        let digit = |byte: u8| (byte as char).to_digit(16);
        name[index] = (digit(pair[0])? * 16 + digit(pair[1])?) as u8;
    }
    Some(name)
}

fn parse_pack_name(name: &str) -> Option<u64> {
    let id = name.strip_suffix(".pack")?;
    (id.len() == 16).then(|| u64::from_str_radix(id, 16).ok()).flatten()
}

/// When a pack was created: its id begins with the milliseconds it was.
fn pack_created(id: u64) -> u64 {
    id >> 16
}

fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |time| time.as_millis() as u64)
}

/// `function` over every item on up to `threads` threads, results in order.
fn parallel<T: Send, R: Send>(items: Vec<T>, threads: usize, function: impl Fn(T) -> R + Sync) -> Vec<R> {
    let threads = threads.clamp(1, items.len().max(1));
    if threads == 1 {
        return items.into_iter().map(function).collect();
    }
    let count = items.len();
    let items: Vec<Mutex<Option<T>>> = items.into_iter().map(|item| Mutex::new(Some(item))).collect();
    let results: Vec<Mutex<Option<R>>> = (0..count).map(|_| Mutex::new(None)).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    // What the helpers allocate is charged to the caller's evaluation.
    let counter = crate::accounting::MemoryTag::current();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let _tag = counter.as_ref().map(crate::accounting::MemoryTag::new);
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if index >= count {
                        break;
                    }
                    if let Some(item) = items[index].lock().unwrap_or_else(|p| p.into_inner()).take() {
                        *results[index].lock().unwrap_or_else(|p| p.into_inner()) = Some(function(item));
                    }
                }
            });
        }
    });
    results.into_iter().map(|slot| slot.into_inner().unwrap_or_else(|p| p.into_inner()).expect("mapped")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temp(PathBuf);

    impl Temp {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "flowlog-store-{label}-{}-{}",
                std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
            ));
            Self(path)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn state(name: &str, rows: Vec<Vec<Val>>) -> Arc<RelationState> {
        Arc::new(RelationState::new(name, rows.first().map_or(1, Vec::len), rows))
    }

    fn entry(relations: &[(&str, Vec<Vec<Val>>)], symbols: Vec<(Val, String)>) -> CacheEntry {
        CacheEntry::with_symbols(
            relations.iter().map(|(name, rows)| (name.to_string(), state(name, rows.clone()))).collect(),
            symbols,
        )
    }

    fn key(byte: u8) -> String {
        format!("{byte:02x}").repeat(32)
    }

    fn heads(names: &[(&str, usize)]) -> BTreeMap<String, usize> {
        names.iter().map(|(name, arity)| (name.to_string(), *arity)).collect()
    }

    #[test]
    fn a_process_reads_what_another_wrote_and_shares_identical_rows() {
        let temp = Temp::new("share");
        let first = DiskStore::new(temp.0.clone(), 1 << 30);
        first.put(&key(1), &entry(&[("H", vec![vec![1, 2], vec![3, 4]]), ("K", vec![vec![7]])], vec![(7, "seven".into())]), Shelf::Units);
        // Same rows under another key and name: one blob serves both.
        first.put(&key(2), &entry(&[("G", vec![vec![1, 2], vec![3, 4]])], vec![]), Shelf::Units);
        first.flush();
        assert_eq!(first.counters().reused_blobs, 1);

        let second = DiskStore::new(temp.0.clone(), 1 << 30);
        let one = heads(&[("H", 2), ("K", 1)]);
        let two = heads(&[("G", 2)]);
        let wrong = heads(&[("H", 2)]);
        let loaded = second.load_many(&[(&key(1), &one), (&key(2), &two), (&key(1), &wrong), (&key(3), &two)], 4);
        let first_entry = loaded[0].as_ref().expect("written entry reads back");
        assert_eq!(*first_entry.relations["H"].rows, vec![vec![1, 2], vec![3, 4]]);
        assert_eq!(*first_entry.relations["K"].rows, vec![vec![7]]);
        assert_eq!(first_entry.symbols, vec![(7, "seven".to_string())]);
        let second_entry = loaded[1].as_ref().expect("shared rows read back");
        assert_eq!(second_entry.relations["G"].name, "G");
        assert_eq!(second_entry.relations["G"].digest, first_entry.relations["H"].digest);
        assert!(loaded[2].is_none(), "heads must match exactly");
        assert!(loaded[3].is_none(), "an unknown key is a miss");
    }

    #[test]
    fn unit_states_are_read_from_unit_packs_only() {
        let temp = Temp::new("shelves");
        let store = DiskStore::new(temp.0.clone(), 1 << 30);
        // A contribution first, then a unit whose rows are the same.
        store.put(&key(1), &entry(&[("C", vec![vec![1], vec![2]])], vec![]), Shelf::Contributions);
        store.put(&key(2), &entry(&[("U", vec![vec![1], vec![2]])], vec![]), Shelf::Units);
        // And a contribution whose rows a unit already holds is not written.
        store.put(&key(3), &entry(&[("D", vec![vec![1], vec![2]])], vec![]), Shelf::Contributions);
        store.flush();
        assert_eq!(store.counters().reused_blobs, 1);
        let packs: Vec<u64> = fs::read_dir(temp.0.join(PACKS))
            .unwrap()
            .map(|item| parse_pack_name(item.unwrap().file_name().to_str().unwrap()).unwrap())
            .collect();
        assert_eq!(packs.len(), 2, "one pack per shelf");
        let reader = DiskStore::new(temp.0.clone(), 1 << 30);
        let loaded = reader.load_many(&[(&key(2), &heads(&[("U", 1)]))], 1);
        assert_eq!(*loaded[0].as_ref().unwrap().relations["U"].rows, vec![vec![1], vec![2]]);
        let touched: Vec<u64> = reader.state().touched.iter().copied().collect();
        assert_eq!(touched.len(), 1);
        assert_eq!(shelf_of(touched[0]), Shelf::Units);
    }

    #[test]
    fn a_damaged_blob_or_a_torn_index_tail_is_a_miss_and_never_wrong() {
        let temp = Temp::new("damage");
        let writer = DiskStore::new(temp.0.clone(), 1 << 30);
        writer.put(&key(1), &entry(&[("H", vec![vec![1], vec![2]])], vec![]), Shelf::Units);
        writer.put(&key(2), &entry(&[("K", vec![vec![5], vec![6]])], vec![]), Shelf::Units);
        writer.flush();
        // A half-written record at the end of the index is ignored.
        let mut log = OpenOptions::new().append(true).open(temp.0.join(INDEX_FILE)).unwrap();
        log.write_all(&[b'F', b'L', b'I', b'X', KIND_ENTRY, 200, 0, 0, 0, 1, 2, 3]).unwrap();
        // Flip a cell of K's blob in its pack.
        let pack = fs::read_dir(temp.0.join(PACKS)).unwrap().next().unwrap().unwrap().path();
        let mut bytes = fs::read(&pack).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&pack, bytes).unwrap();

        let reader = DiskStore::new(temp.0.clone(), 1 << 30);
        let loaded = reader.load_many(&[(&key(1), &heads(&[("H", 1)])), (&key(2), &heads(&[("K", 1)]))], 2);
        assert_eq!(*loaded[0].as_ref().expect("intact entry").relations["H"].rows, vec![vec![1], vec![2]]);
        assert!(loaded[1].is_none(), "a damaged blob must miss");
        // And it stays a miss in this process without another read.
        assert!(reader.load_many(&[(&key(2), &heads(&[("K", 1)]))], 1)[0].is_none());
    }

    #[test]
    fn records_appended_later_are_read_between_lookups() {
        let temp = Temp::new("tail");
        let reader = DiskStore::new(temp.0.clone(), 1 << 30);
        let h = heads(&[("H", 1)]);
        assert!(reader.load_many(&[(&key(9), &h)], 1)[0].is_none());
        let writer = DiskStore::new(temp.0.clone(), 1 << 30);
        writer.put(&key(9), &entry(&[("H", vec![vec![42]])], vec![]), Shelf::Units);
        writer.flush();
        assert_eq!(*reader.load_many(&[(&key(9), &h)], 1)[0].as_ref().unwrap().relations["H"].rows, vec![vec![42]]);
    }

    #[test]
    fn eviction_drops_the_least_recently_used_packs_and_rewrites_the_index() {
        let temp = Temp::new("evict");
        let budget = 1 << 20;
        let big = |base: Val| -> Vec<Vec<Val>> { (0..20_000).map(|row| vec![base + row]).collect() };
        // Three processes, each a pack of about 160 KB, all of them old.
        for (byte, base) in [(1u8, 0), (2, 100_000), (3, 200_000)] {
            let store = DiskStore::new(temp.0.clone(), budget);
            store.put(&key(byte), &entry(&[("H", big(base))], vec![]), Shelf::Units);
            store.flush();
        }
        let packs = || fs::read_dir(temp.0.join(PACKS)).unwrap().count();
        assert_eq!(packs(), 3);
        // Within budget nothing is evicted.
        assert_eq!(DiskStore::new(temp.0.clone(), budget).maintain().disk_files_removed, 0);

        // Key 3 was used most recently.
        let user = DiskStore::new(temp.0.clone(), budget);
        assert!(user.load_many(&[(&key(3), &heads(&[("H", 1)]))], 1)[0].is_some());
        user.flush();
        let small = DiskStore::new(temp.0.clone(), 300 << 10).unprotected();
        let stats = small.maintain();
        assert_eq!(stats.disk_files_removed, 2, "{stats:?}");
        assert_eq!(packs(), 1);
        let after = DiskStore::new(temp.0.clone(), 300 << 10);
        let h = heads(&[("H", 1)]);
        let loaded = after.load_many(&[(&key(1), &h), (&key(2), &h), (&key(3), &h)], 2);
        assert!(loaded[0].is_none() && loaded[1].is_none());
        assert!(loaded[2].is_some(), "the recently used pack survives");
        let index = fs::metadata(temp.0.join(INDEX_FILE)).unwrap().len();
        assert!(index < 1024, "the index holds only live records: {index}");
    }

    #[test]
    fn a_pack_its_writer_still_holds_is_not_evicted() {
        let temp = Temp::new("held");
        let big = |base: Val| -> Vec<Vec<Val>> { (0..20_000).map(|row| vec![base + row]).collect() };
        let live = DiskStore::new(temp.0.clone(), 1 << 30);
        live.put(&key(1), &entry(&[("H", big(0))], vec![]), Shelf::Units);
        live.flush();
        {
            let exited = DiskStore::new(temp.0.clone(), 1 << 30);
            exited.put(&key(2), &entry(&[("H", big(100_000))], vec![]), Shelf::Units);
            exited.flush();
        }
        // Over a budget of one pack: the older pack is still being written,
        // so the newer one, whose writer is gone, is the one removed.
        let stats = DiskStore::new(temp.0.clone(), 200 << 10).unprotected().maintain();
        assert_eq!(stats.disk_files_removed, 1, "{stats:?}");
        let h = heads(&[("H", 1)]);
        let loaded = DiskStore::new(temp.0.clone(), 1 << 30).load_many(&[(&key(1), &h), (&key(2), &h)], 1);
        assert!(loaded[0].is_some() && loaded[1].is_none());
        drop(live);
    }

    #[test]
    fn an_index_rewrite_loses_no_record_appended_around_it() {
        let temp = Temp::new("rewrite");
        let writer = DiskStore::new(temp.0.clone(), 1 << 30);
        writer.put(&key(1), &entry(&[("H", vec![vec![1]])], vec![]), Shelf::Units);
        writer.flush();
        let maintainer = DiskStore::new(temp.0.clone(), 1 << 30);
        let mut state = maintainer.state();
        assert!(maintainer.prepare(&mut state));
        // Appended after the maintainer read the index, before it replaces it.
        writer.put(&key(2), &entry(&[("H", vec![vec![2]])], vec![]), Shelf::Units);
        writer.flush();
        let present: HashSet<u64> = fs::read_dir(temp.0.join(PACKS))
            .unwrap()
            .map(|item| parse_pack_name(item.unwrap().file_name().to_str().unwrap()).unwrap())
            .collect();
        maintainer.rewrite_index(&mut state, &present).unwrap();
        drop(state);
        // Appended after it: the writer follows the new index.
        writer.put(&key(3), &entry(&[("H", vec![vec![3]])], vec![]), Shelf::Units);
        writer.flush();
        let h = heads(&[("H", 1)]);
        let requests = [(key(1), &h), (key(2), &h), (key(3), &h)];
        let requests: Vec<(&str, &BTreeMap<String, usize>)> =
            requests.iter().map(|(key, heads)| (key.as_str(), *heads)).collect();
        let found = |store: &DiskStore| -> Vec<bool> {
            store.load_many(&requests, 1).iter().map(Option::is_some).collect()
        };
        assert_eq!(found(&DiskStore::new(temp.0.clone(), 1 << 30)), vec![true; 3]);
        assert_eq!(found(&writer), vec![true; 3]);
    }

    #[test]
    fn an_empty_layout_name_left_by_a_crash_is_written_again() {
        let temp = Temp::new("format");
        fs::create_dir_all(&temp.0).unwrap();
        fs::write(temp.0.join(FORMAT_FILE), "").unwrap();
        let store = DiskStore::new(temp.0.clone(), 1 << 30);
        store.put(&key(1), &entry(&[("H", vec![vec![1]])], vec![]), Shelf::Units);
        store.flush();
        assert_eq!(fs::read_to_string(temp.0.join(FORMAT_FILE)).unwrap(), FORMAT);
        let h = heads(&[("H", 1)]);
        assert!(DiskStore::new(temp.0.clone(), 1 << 30).load_many(&[(&key(1), &h)], 1)[0].is_some());
    }

    #[test]
    fn a_pack_cut_short_still_serves_the_blobs_before_the_cut() {
        let temp = Temp::new("cut");
        let writer = DiskStore::new(temp.0.clone(), 1 << 30);
        writer.put(&key(1), &entry(&[("H", vec![vec![1], vec![2]])], vec![]), Shelf::Units);
        writer.put(&key(2), &entry(&[("K", vec![vec![5], vec![6]])], vec![]), Shelf::Units);
        writer.flush();
        drop(writer);
        let pack = fs::read_dir(temp.0.join(PACKS)).unwrap().next().unwrap().unwrap().path();
        let length = fs::metadata(&pack).unwrap().len();
        OpenOptions::new().write(true).open(&pack).unwrap().set_len(length - 4).unwrap();
        let reader = DiskStore::new(temp.0.clone(), 1 << 30);
        let loaded = reader.load_many(&[(&key(1), &heads(&[("H", 1)])), (&key(2), &heads(&[("K", 1)]))], 1);
        assert!(loaded[0].is_some(), "the blob before the cut is intact");
        assert!(loaded[1].is_none(), "the blob the cut shortened is a miss");
    }

    #[test]
    fn an_entry_naming_a_head_twice_is_not_the_entry_asked_for() {
        let stored = StoredEntry {
            relations: vec![
                StoredRelation { name: "H".into(), digest: [1; 32], arity: 1, rows: 1 },
                StoredRelation { name: "H".into(), digest: [2; 32], arity: 1, rows: 1 },
            ],
            symbols: None,
        };
        assert!(!holds_exactly(&stored, &heads(&[("H", 1), ("K", 1)])));
        assert!(holds_exactly(
            &StoredEntry { relations: stored.relations[..1].to_vec(), symbols: None },
            &heads(&[("H", 1)])
        ));
    }

    #[test]
    fn records_survive_their_own_framing() {
        let mut out = Vec::new();
        let stored = StoredEntry {
            relations: vec![StoredRelation { name: "H".into(), digest: [3; 32], arity: 2, rows: 9 }],
            symbols: Some([4; 32]),
        };
        let mut payload = Vec::new();
        encode_entry_record(&[1; 32], &stored, &mut payload);
        push_record(&mut out, KIND_ENTRY, &payload);
        out.extend_from_slice(b"garbage");
        payload.clear();
        encode_touch_record(5, &[6, 7], &mut payload);
        push_record(&mut out, KIND_TOUCH, &payload);
        let mut seen = Vec::new();
        let consumed = parse_records(&out, |kind, payload| seen.push((kind, payload.to_vec())));
        assert_eq!(consumed, out.len());
        assert_eq!(seen.len(), 2);
        let (key, entry) = decode_entry_record(&seen[0].1).unwrap();
        assert_eq!(key, [1; 32]);
        assert_eq!(entry.relations[0].rows, 9);
        assert_eq!(decode_touch_record(&seen[1].1), Some((5, vec![6, 7])));
        // A record cut short is left for later; the garbage before it is not.
        assert_eq!(parse_records(&out[..out.len() - 3], |_, _| {}), out.len() - 45);

        // A record a crash cut short in the middle, claiming more than is
        // left of the file, hides nothing that was appended after it.
        let mut torn = Vec::new();
        torn.extend_from_slice(&RECORD_MAGIC);
        torn.push(KIND_ENTRY);
        torn.extend_from_slice(&(1u32 << 20).to_le_bytes());
        torn.extend_from_slice(b"partial");
        let mut later = Vec::new();
        push_record(&mut later, KIND_TOUCH, &payload);
        torn.extend_from_slice(&later);
        let mut seen = 0;
        assert_eq!(parse_records(&torn, |_, _| seen += 1), torn.len());
        assert_eq!(seen, 1);
    }
}

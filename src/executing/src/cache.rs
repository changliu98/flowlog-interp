//! Content-addressed cache of materialized relation states.
//!
//! A Differential Dataflow graph has fixed topology: arbitrary edits cannot be
//! spliced into an already assembled recursive scope. Instead, evaluation
//! reads along the strata and, for each *unit* -- one recursive stratum, or one
//! relation's rules within a non-recursive stratum -- asks this cache for the
//! state its rules derive from the states it reads. A unit's key is content on
//! both sides: the canonical text of its rules (what they mean, not how they
//! were written), the column types of the relations it touches, the source of
//! every embedded block a rule of the unit calls into, and the digest of every
//! input relation's rows. An upstream edit that leaves a relation's rows
//! unchanged therefore stops invalidating there, a rule rewritten to mean the
//! same thing keeps its entries, an edit to a block no rule of the unit calls
//! touches nothing, and an unrelated declaration or clause elsewhere in the
//! program touches nothing. On an ordinary non-recursive unit miss, individual
//! rule contributions can be reused from this same store before assembling
//! the head's set union. Contribution keys exclude inherited head rows and
//! have their own domain.
//!
//! The cache is two tiers with one key: a process-resident tier bounded by
//! estimated bytes and evicted least recently used, and an optional on-disk
//! store shared by every process that points at the same directory, bounded
//! by bytes and swept by access time. The memory tier is the default; the
//! disk tier is a spill a caller asks for. Neither is a source of truth. A
//! miss costs one ordinary evaluation; a hit is rows that a run with identical
//! rules and identical inputs derived, and every entry read back from disk is
//! checked against the digest it was written with.
//!
//! An entry that holds symbol cells carries their texts, so a process that
//! never interned them can still name them: the entry is self-describing.
//!
//! What the memo is not: it does not retain dataflow topology, and it does
//! not incrementally update a recursive fixed point. A missed recursive or
//! aggregate unit is recomputed whole.

use crate::canonical::canonical_rules;
use parsing::decl::DataType;
use parsing::parser::Program;
use parsing::rule::{FLRule, Predicate};
use parsing::Val;
use reading::SEMIRING_TYPE;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read, Seek, Write};
use std::ops::AddAssign;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The version of the key: bumped whenever the key's inputs change meaning.
pub const CACHE_ABI: &str = "flowlog-interp/state-cache/v3";
/// The version of the on-disk entry format.
pub const STATE_MAGIC: &[u8; 8] = b"FLSTATE3";
const DISK_SWEEP_EVERY: usize = 64;
const DISK_SWEEP_INTERVAL: Duration = Duration::from_secs(1);
const DISK_SWEEP_BATCH: usize = 4096;
const DISK_SHARDS: usize = 256;

// ------------------------------------------------------------------ states

/// One relation's rows at a point of the evaluation, named by their digest.
///
/// Rows are sorted and distinct, so the digest is a function of the set and
/// not of the order the engine happened to emit it in.
#[derive(Debug)]
pub struct RelationState {
    pub name: String,
    pub arity: usize,
    pub rows: Arc<Vec<Vec<Val>>>,
    pub digest: [u8; 32],
}

impl RelationState {
    pub fn new(name: &str, arity: usize, mut rows: Vec<Vec<Val>>) -> Self {
        rows.sort_unstable();
        rows.dedup();
        let digest = rows_digest(arity, &rows);
        Self {
            name: name.to_string(),
            arity,
            rows: Arc::new(rows),
            digest,
        }
    }

    pub fn empty(name: &str, arity: usize) -> Self {
        Self::new(name, arity, Vec::new())
    }

    /// Rows already sorted and distinct, with the digest they were stored under.
    fn from_sorted(name: String, arity: usize, rows: Vec<Vec<Val>>, digest: [u8; 32]) -> Self {
        Self {
            name,
            arity,
            rows: Arc::new(rows),
            digest,
        }
    }

    pub fn digest_hex(&self) -> String {
        hex(&self.digest)
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.name.capacity()
            + self.rows.capacity() * std::mem::size_of::<Vec<Val>>()
            + self
                .rows
                .iter()
                .map(|row| row.capacity() * std::mem::size_of::<Val>())
                .sum::<usize>()
    }
}

fn rows_digest(arity: usize, rows: &[Vec<Val>]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update((arity as u64).to_le_bytes());
    hasher.update((rows.len() as u64).to_le_bytes());
    for row in rows {
        for value in row {
            hasher.update(value.to_le_bytes());
        }
    }
    hasher.finalize().into()
}

/// The states one unit derives: every head of the unit after it ran, plus
/// the texts of the symbols those rows hold.
#[derive(Debug)]
pub struct CacheEntry {
    pub relations: BTreeMap<String, Arc<RelationState>>,
    pub symbols: Vec<(Val, String)>,
    bytes: usize,
}

impl CacheEntry {
    pub fn new(relations: BTreeMap<String, Arc<RelationState>>) -> Self {
        Self::with_symbols(relations, Vec::new())
    }

    pub fn with_symbols(
        relations: BTreeMap<String, Arc<RelationState>>,
        symbols: Vec<(Val, String)>,
    ) -> Self {
        let bytes = std::mem::size_of::<Self>()
            + relations
                .iter()
                .map(|(name, relation)| name.capacity() + relation.estimated_bytes())
                .sum::<usize>()
            + symbols
                .iter()
                .map(|(_, text)| text.capacity() + 16)
                .sum::<usize>();
        Self {
            relations,
            symbols,
            bytes,
        }
    }

    pub fn row_count(&self) -> usize {
        self.relations
            .values()
            .map(|relation| relation.rows.len())
            .sum()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    fn matches(&self, heads: &BTreeMap<String, usize>) -> bool {
        self.relations.len() == heads.len()
            && heads.iter().all(|(name, arity)| {
                self.relations
                    .get(name)
                    .is_some_and(|relation| relation.arity == *arity)
            })
    }
}

/// The distinct symbol cells of a relation's rows, by its column types.
pub fn symbol_cells(rows: &[Vec<Val>], types: &[DataType]) -> Vec<Val> {
    let columns = types
        .iter()
        .enumerate()
        .filter_map(|(index, column)| column.is_symbol().then_some(index))
        .collect::<Vec<_>>();
    if columns.is_empty() {
        return Vec::new();
    }
    let mut cells = BTreeSet::new();
    for row in rows {
        for &column in &columns {
            if let Some(cell) = row.get(column) {
                cells.insert(*cell);
            }
        }
    }
    cells.into_iter().collect()
}

// ------------------------------------------------------------------- units

/// The cache unit: one recursive stratum, or one relation's rules within a
/// non-recursive stratum.
///
/// Heads of one non-recursive stratum are independent of one another, so each
/// head's rules are keyed and reused on their own; the rules of a recursive
/// stratum define each other and are one unit with every head it has.
pub struct Unit<'a> {
    pub heads: BTreeMap<String, usize>,
    pub rules: Vec<&'a FLRule>,
    pub recursive: bool,
}

impl Unit<'_> {
    /// The embedded functions the unit's rules call, each once.
    pub fn called_functions(&self) -> Vec<&str> {
        let mut names = Vec::new();
        for rule in &self.rules {
            for call in rule.calls() {
                let name = call.call().function();
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names
    }
}

pub fn units_of_stratum<'a>(rules: &[&'a FLRule], recursive: bool) -> Vec<Unit<'a>> {
    if recursive {
        let heads = rules
            .iter()
            .map(|rule| (rule.head().name().clone(), rule.head().arity()))
            .collect();
        return vec![Unit {
            heads,
            rules: rules.to_vec(),
            recursive: true,
        }];
    }
    let mut order = Vec::new();
    let mut by_head: HashMap<String, Vec<&'a FLRule>> = HashMap::new();
    for &rule in rules {
        let name = rule.head().name().clone();
        if !by_head.contains_key(&name) {
            order.push(name.clone());
        }
        by_head.entry(name).or_default().push(rule);
    }
    order
        .into_iter()
        .map(|name| {
            let rules = by_head.remove(&name).expect("head collected above");
            let arity = rules[0].head().arity();
            Unit {
                heads: BTreeMap::from([(name, arity)]),
                rules,
                recursive: false,
            }
        })
        .collect()
}

/// The states a unit reads: every body relation that is not one of its own
/// heads, plus any head that already has a state (a relation this unit adds
/// rows to, whose earlier rows are an input of the union it derives).
///
/// A relation nothing has defined yet is an empty state at its arity, which
/// validation has fixed for every relation the program mentions.
pub fn unit_inputs(
    unit: &Unit<'_>,
    states: &HashMap<String, Arc<RelationState>>,
    program: &Program,
) -> BTreeMap<String, Arc<RelationState>> {
    let mut names = BTreeSet::new();
    for rule in &unit.rules {
        for predicate in rule.rhs() {
            if let Predicate::AtomPredicate(atom) | Predicate::NegatedAtomPredicate(atom) =
                predicate
            {
                if !unit.heads.contains_key(atom.name()) {
                    names.insert(atom.name().to_string());
                }
            }
        }
    }
    for head in unit.heads.keys() {
        if states.contains_key(head) {
            names.insert(head.clone());
        }
    }
    names
        .into_iter()
        .map(|name| {
            let state = match states.get(&name) {
                Some(state) => Arc::clone(state),
                None => {
                    let arity = program
                        .column_types(&name)
                        .map(<[DataType]>::len)
                        .unwrap_or_else(|| {
                            panic!("relation {name} is read before validation typed it")
                        });
                    Arc::new(RelationState::empty(&name, arity))
                }
            };
            (name, state)
        })
        .collect()
}

/// Everything the unit's states depend on, and nothing else.
pub fn unit_key(
    unit: &Unit<'_>,
    inputs: &BTreeMap<String, Arc<RelationState>>,
    program: &Program,
) -> String {
    let mut hasher = KeyHasher::new();
    hasher.add(CACHE_ABI);
    hasher.add(SEMIRING_TYPE);
    hasher.add(&canonical_rules(&unit.rules));

    let mut touched = unit
        .heads
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    touched.extend(inputs.keys().map(String::as_str));
    for name in touched {
        hasher.add(&declaration_form(name, program));
    }

    // Only the blocks the unit calls into: an edit to any other block leaves
    // the unit's meaning, and its key, where they were.
    let called = unit.called_functions();
    if !called.is_empty() {
        if let Some(embedded) = program.embedded_rust() {
            for index in embedded.blocks_defining(called.iter().copied()) {
                hasher.add("embedded-rust-block");
                hasher.add(embedded.block(index).source());
            }
        }
    }

    for (name, state) in inputs {
        hasher.add(name);
        hasher.add_bytes(&state.digest);
    }
    hasher.finish()
}

/// One ordinary non-recursive rule's output, before union with an inherited
/// head or any other rule. `inputs` contains only the rule's body relations.
/// Keep this namespace separate from complete unit states.
pub fn contribution_key(
    unit: &Unit<'_>,
    inputs: &BTreeMap<String, Arc<RelationState>>,
    program: &Program,
) -> String {
    assert!(!unit.recursive && unit.rules.len() == 1);
    assert!(unit.heads.keys().all(|head| !inputs.contains_key(head)));
    let mut hasher = KeyHasher::new();
    hasher.add("flowlog-interp/rule-contribution/v2");
    hasher.add(&unit_key(unit, inputs, program));
    hasher.finish()
}

/// A relation as the key sees it: its name and its column types. Attribute
/// names and input paths are spelling.
fn declaration_form(name: &str, program: &Program) -> String {
    match program.column_types(name) {
        Some(columns) => format!(
            "{name}({})",
            columns
                .iter()
                .map(|column| column.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
        None => format!("{name}(<untyped>)"),
    }
}

// ------------------------------------------------------------------- stats

/// The counters of one run through the cache. Every count is exact and is
/// defined here:
///
/// - `strata`: strata the program stratified into.
/// - `units`: units the run keyed (one recursive stratum, or one head of a
///   non-recursive stratum).
/// - `hits` / `misses`: units served from the cache / evaluated.
/// - `disk_hits`: hits served from the disk tier rather than memory.
/// - `hits_after_miss`: hits keyed after at least one earlier unit missed in
///   this run. Reported as `cutoff_hits` for compatibility; it counts every
///   later hit, independent units included, so it is an upper bound on early
///   cutoff and not a causal count.
/// - `contribution_*`: rule-level lookups inside missed ordinary units.
/// - `rules_evaluated`: source rules assembled into dataflows (before any
///   sideways expansion).
/// - `rows_loaded` / `rows_cached`: rows served from entries / rows stored
///   into entries, whole units only; contributions count separately.
/// - `*_micros`: wall time of preparation, dataflow, cache bookkeeping,
///   output, and the whole run.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CacheRunStats {
    pub strata: usize,
    pub units: usize,
    pub hits: usize,
    pub misses: usize,
    pub disk_hits: usize,
    #[serde(rename = "cutoff_hits")]
    pub hits_after_miss: usize,
    pub contribution_hits: usize,
    pub contribution_misses: usize,
    pub contribution_disk_hits: usize,
    pub contribution_rows_loaded: usize,
    pub contribution_rows_cached: usize,
    pub rules_evaluated: usize,
    pub rows_loaded: usize,
    pub rows_cached: usize,
    pub planning_micros: u64,
    pub execution_micros: u64,
    pub cache_micros: u64,
    pub output_micros: u64,
    pub total_micros: u64,
    #[serde(flatten)]
    pub disk: DiskSweepStats,
    pub entries: usize,
    pub resident_rows: usize,
    pub resident_bytes: usize,
    pub max_bytes: usize,
    /// Bytes the evaluation's threads held at most, as the allocator saw it.
    pub peak_memory_bytes: i64,
    /// Rows materialized at unit boundaries.
    pub materialized_rows: u64,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct DiskSweepStats {
    pub disk_sweep_micros: u64,
    pub disk_sweeps: usize,
    pub disk_sweep_skips: usize,
    pub disk_files_examined: usize,
    pub disk_files_removed: usize,
    pub disk_bytes_removed: u64,
}

impl AddAssign for DiskSweepStats {
    fn add_assign(&mut self, other: Self) {
        self.disk_sweep_micros += other.disk_sweep_micros;
        self.disk_sweeps += other.disk_sweeps;
        self.disk_sweep_skips += other.disk_sweep_skips;
        self.disk_files_examined += other.disk_files_examined;
        self.disk_files_removed += other.disk_files_removed;
        self.disk_bytes_removed += other.disk_bytes_removed;
    }
}

/// The memory tier's occupancy, maintained on every insert and eviction.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CacheStateStats {
    pub entries: usize,
    pub resident_rows: usize,
    pub resident_bytes: usize,
    pub max_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitSource {
    Memory,
    Disk,
}

// ------------------------------------------------------------------- store

#[derive(Debug)]
struct StoredEntry {
    entry: Arc<CacheEntry>,
    last_used: u64,
}

/// The two-tier cache behind one key.
#[derive(Debug)]
pub struct StrataCache {
    entries: HashMap<String, StoredEntry>,
    /// (last_used, key), oldest first: the eviction order.
    order: BTreeSet<(u64, String)>,
    max_bytes: usize,
    resident_bytes: usize,
    resident_rows: usize,
    clock: u64,
    disk: Option<DiskStore>,
}

impl StrataCache {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: BTreeSet::new(),
            max_bytes,
            resident_bytes: 0,
            resident_rows: 0,
            clock: 0,
            disk: None,
        }
    }

    pub fn with_disk(mut self, disk: DiskStore) -> Self {
        self.disk = Some(disk);
        self
    }

    pub fn has_disk(&self) -> bool {
        self.disk.is_some()
    }

    pub fn state_stats(&self) -> CacheStateStats {
        CacheStateStats {
            entries: self.entries.len(),
            resident_rows: self.resident_rows,
            resident_bytes: self.resident_bytes,
            max_bytes: self.max_bytes,
        }
    }

    /// The entry under `key`, if one exists whose heads are exactly `heads`.
    pub fn lookup(
        &mut self,
        key: &str,
        heads: &BTreeMap<String, usize>,
    ) -> Option<(Arc<CacheEntry>, HitSource)> {
        self.clock = self.clock.wrapping_add(1);
        if let Some(stored) = self.entries.get_mut(key) {
            if stored.entry.matches(heads) {
                self.order.remove(&(stored.last_used, key.to_string()));
                stored.last_used = self.clock;
                self.order.insert((self.clock, key.to_string()));
                return Some((Arc::clone(&stored.entry), HitSource::Memory));
            }
        }
        let entry = self.disk.as_ref()?.get(key)?;
        if !entry.matches(heads) {
            return None;
        }
        let entry = Arc::new(entry);
        self.retain(key.to_string(), Arc::clone(&entry));
        Some((entry, HitSource::Disk))
    }

    /// Record a unit's states: on disk when a store is configured, and in
    /// memory when the entry fits the budget.
    pub fn insert(&mut self, key: String, entry: Arc<CacheEntry>) -> DiskSweepStats {
        let maintenance = self
            .disk
            .as_mut()
            .map(|disk| disk.put(&key, &entry))
            .unwrap_or_default();
        self.retain(key, entry);
        maintenance
    }

    fn remove(&mut self, key: &str) {
        if let Some(previous) = self.entries.remove(key) {
            self.order.remove(&(previous.last_used, key.to_string()));
            self.resident_bytes = self.resident_bytes.saturating_sub(previous.entry.bytes);
            self.resident_rows = self.resident_rows.saturating_sub(previous.entry.row_count());
        }
    }

    fn retain(&mut self, key: String, entry: Arc<CacheEntry>) {
        if self.max_bytes == 0 || entry.bytes > self.max_bytes {
            return;
        }
        self.clock = self.clock.wrapping_add(1);
        self.remove(&key);
        self.resident_bytes += entry.bytes;
        self.resident_rows += entry.row_count();
        self.order.insert((self.clock, key.clone()));
        self.entries.insert(
            key,
            StoredEntry {
                entry,
                last_used: self.clock,
            },
        );
        while self.resident_bytes > self.max_bytes {
            let Some((_, oldest)) = self.order.iter().next().cloned() else {
                break;
            };
            self.remove(&oldest);
        }
    }
}

// -------------------------------------------------------------------- disk

/// A directory of states, one file per key, shared by every process that
/// points at it. Writes land whole (temporary file, then rename) and reads
/// verify the digest each relation was written with, so a peer's half-written
/// or damaged file is a miss and never a wrong answer.
#[derive(Debug)]
pub struct DiskStore {
    root: PathBuf,
    max_bytes: u64,
    writes_since_sweep: usize,
    next_sweep_check: Instant,
}

/// Approximate accounting and a resumable cursor, under one stable lock inode.
/// A partial/corrupt record only loses cache accounting: entry validity and
/// query results never depend on it, and later slices rebuild the estimates.
#[derive(Debug, Serialize, Deserialize)]
struct DiskSweepState {
    version: u32,
    last_started_millis: u64,
    shard: usize,
    after: String,
    scanned_bytes: u64,
    shard_bytes: Vec<u64>,
}

impl Default for DiskSweepState {
    fn default() -> Self {
        Self {
            version: 1,
            last_started_millis: 0,
            shard: 0,
            after: String::new(),
            scanned_bytes: 0,
            shard_bytes: vec![0; DISK_SHARDS],
        }
    }
}

impl DiskStore {
    pub fn new(root: PathBuf, max_bytes: u64) -> Self {
        Self {
            root,
            max_bytes,
            writes_since_sweep: 0,
            next_sweep_check: Instant::now(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, key: &str) -> PathBuf {
        self.root.join(&key[..2]).join(format!("{key}.state"))
    }

    fn get(&self, key: &str) -> Option<CacheEntry> {
        let path = self.path(key);
        let bytes = fs::read(&path).ok()?;
        match decode_entry(&bytes) {
            Some(entry) => Some(entry),
            None => {
                let _ = fs::remove_file(&path);
                None
            }
        }
    }

    fn put(&mut self, key: &str, entry: &CacheEntry) -> DiskSweepStats {
        let path = self.path(key);
        if let Err(error) = self.write(&path, entry) {
            tracing::warn!("state store could not write {}: {error}", path.display());
            return DiskSweepStats::default();
        }
        self.writes_since_sweep = self.writes_since_sweep.saturating_add(1);
        if self.writes_since_sweep >= DISK_SWEEP_EVERY && Instant::now() >= self.next_sweep_check {
            self.writes_since_sweep = 0;
            self.next_sweep_check = Instant::now() + DISK_SWEEP_INTERVAL;
            let started = Instant::now();
            let mut stats = DiskSweepStats::default();
            if let Err(error) = self.sweep(&mut stats) {
                tracing::warn!(
                    "state store maintenance at {}: {error}",
                    self.root.display()
                );
            }
            stats.disk_sweep_micros =
                u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
            return stats;
        }
        DiskSweepStats::default()
    }

    fn write(&self, path: &Path, entry: &CacheEntry) -> io::Result<()> {
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "state path has no parent")
        })?;
        fs::create_dir_all(parent)?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let temporary = parent.join(format!(".tmp-{}-{nonce}", std::process::id()));
        let result = (|| {
            let mut file = fs::File::create(&temporary)?;
            file.write_all(&encode_entry(entry))?;
            file.sync_all()?;
            fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    /// One coordinated slice, never a walk of the whole store. Directory
    /// names are read from one shard; at most DISK_SWEEP_BATCH files are
    /// statted or evicted. Progress survives short-lived engine processes.
    fn sweep(&self, stats: &mut DiskSweepStats) -> io::Result<()> {
        let mut lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join(".sweep.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                stats.disk_sweep_skips += 1;
                return Ok(());
            }
            Err(fs::TryLockError::Error(error)) => return Err(error),
        }
        let mut bytes = Vec::new();
        (&mut lock).take(16 * 1024).read_to_end(&mut bytes)?;
        let mut state = serde_json::from_slice::<DiskSweepState>(&bytes)
            .ok()
            .filter(|state| {
                state.version == 1
                    && state.shard < DISK_SHARDS
                    && state.shard_bytes.len() == DISK_SHARDS
                    && state.after.len() <= 70
            })
            .unwrap_or_default();
        let now = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        if now >= state.last_started_millis
            && now - state.last_started_millis < DISK_SWEEP_INTERVAL.as_millis() as u64
        {
            stats.disk_sweep_skips += 1;
            return Ok(());
        }
        state.last_started_millis = now;
        let prefix = format!("{:02x}", state.shard);
        let shard = self.root.join(&prefix);
        let mut names = Vec::new();
        match fs::read_dir(&shard) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    // Only our own SHA-256 state filenames, never temporary
                    // writes, coordination files or unrelated directory data.
                    if name.len() == 70
                        && name.is_ascii()
                        && name.starts_with(&prefix)
                        && name.ends_with(".state")
                        && name[..64].bytes().all(|byte| byte.is_ascii_hexdigit())
                        && name > state.after
                    {
                        names.push(name);
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        names.sort_unstable();
        let finished = names.len() <= DISK_SWEEP_BATCH;
        let mut files = Vec::new();
        for name in names.into_iter().take(DISK_SWEEP_BATCH) {
            state.after = name.clone();
            stats.disk_files_examined += 1;
            let path = shard.join(name);
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let used = metadata
                .accessed()
                .or_else(|_| metadata.modified())
                .unwrap_or(UNIX_EPOCH);
            state.scanned_bytes = state.scanned_bytes.saturating_add(metadata.len());
            files.push((used, metadata.len(), path));
        }
        // Partial scans are lower bounds. Completed scans replace old
        // estimates; concurrent writes are accounted on the next visit.
        state.shard_bytes[state.shard] = if finished {
            state.scanned_bytes
        } else {
            state.shard_bytes[state.shard].max(state.scanned_bytes)
        };
        let mut total = state
            .shard_bytes
            .iter()
            .copied()
            .fold(0u64, u64::saturating_add);
        files.sort_unstable();
        for (_, size, path) in files {
            if total <= self.max_bytes {
                break;
            }
            if fs::remove_file(path).is_ok() {
                total = total.saturating_sub(size);
                state.shard_bytes[state.shard] =
                    state.shard_bytes[state.shard].saturating_sub(size);
                state.scanned_bytes = state.scanned_bytes.saturating_sub(size);
                stats.disk_files_removed += 1;
                stats.disk_bytes_removed += size;
            }
        }
        if finished {
            state.shard = (state.shard + 1) % DISK_SHARDS;
            state.after.clear();
            state.scanned_bytes = 0;
        }
        let bytes = serde_json::to_vec(&state).map_err(io::Error::other)?;
        lock.rewind()?;
        lock.write_all(&bytes)?;
        lock.set_len(bytes.len() as u64)?;
        stats.disk_sweeps += 1;
        // Dropping the File releases the OS lock, including on errors/panic.
        Ok(())
    }
}

fn encode_entry(entry: &CacheEntry) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(STATE_MAGIC);
    out.extend_from_slice(&(entry.relations.len() as u32).to_le_bytes());
    for (name, state) in &entry.relations {
        out.extend_from_slice(&(name.len() as u32).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(state.arity as u32).to_le_bytes());
        out.extend_from_slice(&(state.rows.len() as u64).to_le_bytes());
        out.extend_from_slice(&state.digest);
        for row in state.rows.iter() {
            for value in row {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
    out.extend_from_slice(&(entry.symbols.len() as u64).to_le_bytes());
    for (id, text) in &entry.symbols {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
    }
    out
}

fn decode_entry(bytes: &[u8]) -> Option<CacheEntry> {
    struct Reader<'a> {
        bytes: &'a [u8],
        at: usize,
    }
    impl<'a> Reader<'a> {
        fn take(&mut self, count: usize) -> Option<&'a [u8]> {
            let end = self.at.checked_add(count)?;
            let slice = self.bytes.get(self.at..end)?;
            self.at = end;
            Some(slice)
        }
        fn u32(&mut self) -> Option<u32> {
            Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
        }
        fn u64(&mut self) -> Option<u64> {
            Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
        }
        fn i64(&mut self) -> Option<i64> {
            Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
        }
    }

    let mut reader = Reader { bytes, at: 0 };
    if reader.take(STATE_MAGIC.len())? != STATE_MAGIC {
        return None;
    }
    let count = reader.u32()? as usize;
    let mut relations = BTreeMap::new();
    for _ in 0..count {
        let name_len = reader.u32()? as usize;
        let name = std::str::from_utf8(reader.take(name_len)?)
            .ok()?
            .to_string();
        let arity = reader.u32()? as usize;
        let row_count = usize::try_from(reader.u64()?).ok()?;
        let mut digest = [0u8; 32];
        digest.copy_from_slice(reader.take(32)?);
        let cells = row_count.checked_mul(arity)?;
        let payload = reader.take(cells.checked_mul(8)?)?;
        let mut rows = Vec::with_capacity(row_count);
        for row in payload.chunks_exact(arity.max(1) * 8).take(row_count) {
            rows.push(
                row.chunks_exact(8)
                    .map(|cell| Val::from_le_bytes(cell.try_into().expect("8-byte cell")))
                    .collect::<Vec<_>>(),
            );
        }
        if arity == 0 {
            rows = vec![Vec::new(); row_count.min(1)];
        }
        if rows_digest(arity, &rows) != digest {
            return None;
        }
        relations.insert(
            name.clone(),
            Arc::new(RelationState::from_sorted(name, arity, rows, digest)),
        );
    }
    let symbol_count = usize::try_from(reader.u64()?).ok()?;
    let mut symbols = Vec::with_capacity(symbol_count.min(1 << 16));
    for _ in 0..symbol_count {
        let id = reader.i64()?;
        let length = reader.u32()? as usize;
        let text = std::str::from_utf8(reader.take(length)?).ok()?.to_string();
        symbols.push((id, text));
    }
    if reader.at != bytes.len() {
        return None;
    }
    Some(CacheEntry::with_symbols(relations, symbols))
}

// ----------------------------------------------------------------- hashing

struct KeyHasher(Sha256);

impl KeyHasher {
    fn new() -> Self {
        Self(Sha256::new())
    }

    fn add(&mut self, value: &str) {
        self.add_bytes(value.as_bytes());
    }

    fn add_bytes(&mut self, value: &[u8]) {
        self.0.update((value.len() as u64).to_le_bytes());
        self.0.update(value);
    }

    fn finish(self) -> String {
        hex(&self.0.finalize())
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut result, "{byte:02x}").expect("writing to a String cannot fail");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(name: &str, rows: Vec<Vec<Val>>) -> Arc<RelationState> {
        Arc::new(RelationState::new(
            name,
            rows.first().map_or(1, Vec::len),
            rows,
        ))
    }

    #[test]
    fn a_state_is_named_by_its_set_of_rows() {
        let a = state("R", vec![vec![1, 2], vec![3, 4]]);
        let b = state("R", vec![vec![3, 4], vec![1, 2], vec![3, 4]]);
        assert_eq!(a.digest, b.digest);
        assert_eq!(a.rows, b.rows);
        let c = state("R", vec![vec![1, 2]]);
        assert_ne!(a.digest, c.digest);
    }

    #[test]
    fn length_prefixes_keep_component_boundaries_distinct() {
        let mut first = KeyHasher::new();
        first.add("ab");
        first.add("c");
        let mut second = KeyHasher::new();
        second.add("a");
        second.add("bc");
        assert_ne!(first.finish(), second.finish());
    }

    #[test]
    fn an_oversized_entry_is_not_retained() {
        let mut cache = StrataCache::new(1);
        cache.insert(
            "key".to_string(),
            Arc::new(CacheEntry::new(BTreeMap::new())),
        );
        assert_eq!(cache.state_stats().entries, 0);
    }

    #[test]
    fn eviction_is_least_recently_used_and_the_stats_follow() {
        let entry = |name: &str| {
            Arc::new(CacheEntry::new(BTreeMap::from([(
                name.to_string(),
                state(name, vec![vec![1], vec![2]]),
            )])))
        };
        let one = entry("A").bytes();
        let mut cache = StrataCache::new(one * 2 + 8);
        cache.insert("a".to_string(), entry("A"));
        cache.insert("b".to_string(), entry("B"));
        assert_eq!(cache.state_stats().entries, 2);
        assert_eq!(cache.state_stats().resident_rows, 4);
        // touch a, so b is the oldest
        let heads = BTreeMap::from([("A".to_string(), 1usize)]);
        assert!(cache.lookup("a", &heads).is_some());
        cache.insert("c".to_string(), entry("C"));
        assert!(cache.entries.contains_key("a"));
        assert!(!cache.entries.contains_key("b"));
        assert!(cache.entries.contains_key("c"));
        assert_eq!(cache.state_stats().resident_rows, 4);
        assert_eq!(cache.order.len(), 2);
    }

    #[test]
    fn a_disk_entry_round_trips_with_its_symbols_and_a_damaged_one_is_a_miss() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "flowlog-state-store-{}-{nonce}",
            std::process::id()
        ));
        let mut store = DiskStore::new(root.clone(), 1 << 30);
        let relations = BTreeMap::from([
            ("H".to_string(), state("H", vec![vec![1, 2], vec![3, 4]])),
            ("K".to_string(), state("K", vec![vec![7]])),
        ]);
        let entry = CacheEntry::with_symbols(relations, vec![(7, "seven".to_string())]);
        let key = "ab".repeat(32);
        store.put(&key, &entry);

        let read = store.get(&key).expect("stored entry reads back");
        assert_eq!(read.relations["H"].rows, entry.relations["H"].rows);
        assert_eq!(read.relations["K"].digest, entry.relations["K"].digest);
        assert_eq!(read.symbols, vec![(7, "seven".to_string())]);

        let path = store.path(&key);
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&path, bytes).unwrap();
        assert!(store.get(&key).is_none(), "a damaged entry must miss");
        assert!(!path.exists(), "a damaged entry is removed");
        let _ = fs::remove_dir_all(root);
    }
}

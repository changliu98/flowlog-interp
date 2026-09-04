//! Content-addressed cache of materialized relation states.
//!
//! A Differential Dataflow graph has fixed topology: arbitrary edits cannot be
//! spliced into an already assembled recursive scope.  Instead, evaluation
//! reads along the strata and, for each *unit* -- one recursive stratum, or one
//! relation's rules within a non-recursive stratum -- asks this cache for the
//! state its rules derive from the states it reads.  A unit's key is content on
//! both sides: the canonical text of its rules (what they mean, not how they
//! were written) and the digest of every input relation's rows.  An upstream
//! edit that leaves a relation's rows unchanged therefore stops invalidating
//! there, a rule rewritten to mean the same thing keeps its entries, and an
//! unrelated declaration or clause elsewhere in the program touches nothing.
//!
//! The cache is two layers with one key: a process-resident LRU bounded by
//! estimated bytes, and an optional on-disk store shared by every process that
//! points at the same directory, bounded by bytes and swept by access time.
//! Neither is a source of truth.  A miss costs one ordinary evaluation; a hit is
//! rows that a run with identical rules and identical inputs derived, and every
//! entry read back from disk is checked against the digest it was written with.

use crate::canonical::canonical_rules;
use parsing::decl::RelDecl;
use parsing::parser::Program;
use parsing::rule::{FLRule, Predicate};
use parsing::Val;
use reading::SEMIRING_TYPE;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const CACHE_ABI: &str = "flowlog-interp/state-cache/v2";
const STATE_MAGIC: &[u8; 8] = b"FLSTATE2";
const DISK_SWEEP_EVERY: usize = 64;

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

/// The states one unit derives: every head of the unit after it ran.
#[derive(Debug)]
pub struct CacheEntry {
    pub relations: BTreeMap<String, Arc<RelationState>>,
    bytes: usize,
}

impl CacheEntry {
    pub fn new(relations: BTreeMap<String, Arc<RelationState>>) -> Self {
        let bytes = std::mem::size_of::<Self>()
            + relations
                .iter()
                .map(|(name, relation)| name.capacity() + relation.estimated_bytes())
                .sum::<usize>();
        Self { relations, bytes }
    }

    pub fn row_count(&self) -> usize {
        self.relations
            .values()
            .map(|relation| relation.rows.len())
            .sum()
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
/// A declared relation nothing has defined yet is an empty state; an undeclared
/// one is refused by name, as the dataflow would refuse it.
pub fn unit_inputs(
    unit: &Unit<'_>,
    states: &HashMap<String, Arc<RelationState>>,
    declarations: &HashMap<&str, &RelDecl>,
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
                    let declaration = declarations.get(name.as_str()).unwrap_or_else(|| {
                        panic!("relation {name} is read before any rule or input defines it")
                    });
                    Arc::new(RelationState::new(&name, declaration.arity(), Vec::new()))
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
    declarations: &HashMap<&str, &RelDecl>,
    program: &Program,
) -> String {
    let mut hasher = KeyHasher::new();
    hasher.add(CACHE_ABI);
    hasher.add(SEMIRING_TYPE);
    hasher.add(&canonical_rules(&unit.rules));

    let mut touched = unit.heads.keys().map(String::as_str).collect::<BTreeSet<_>>();
    touched.extend(inputs.keys().map(String::as_str));
    for name in touched {
        hasher.add(&declaration_form(name, declarations));
    }

    let calls = unit.rules.iter().any(|rule| {
        rule.rhs()
            .iter()
            .any(|predicate| matches!(predicate, Predicate::CallPredicate(_)))
    });
    if calls {
        hasher.add("embedded-rust");
        hasher.add(program.embedded_rust().map_or("", |embedded| embedded.source()));
    }

    for (name, state) in inputs {
        hasher.add(name);
        hasher.add_bytes(&state.digest);
    }
    hasher.finish()
}

/// A declaration as the key sees it: the name and the attribute types.  The
/// attribute names and the input path are spelling.
fn declaration_form(name: &str, declarations: &HashMap<&str, &RelDecl>) -> String {
    match declarations.get(name) {
        Some(declaration) => format!(
            "{name}({})",
            declaration
                .attributes()
                .iter()
                .map(|attribute| attribute.data_type().to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
        None => format!("{name}(<undeclared>)"),
    }
}

// ------------------------------------------------------------------- stats

#[derive(Debug, Clone, Default, Serialize)]
pub struct CacheRunStats {
    pub strata: usize,
    pub units: usize,
    pub hits: usize,
    pub misses: usize,
    /// Hits served from the on-disk store rather than process memory.
    pub disk_hits: usize,
    /// Hits that followed a miss in the same run: units whose inputs an
    /// upstream recomputation left unchanged.  Identity-keyed caching loses
    /// exactly these.
    pub cutoff_hits: usize,
    pub rows_loaded: usize,
    pub rows_cached: usize,
    pub planning_micros: u64,
    pub execution_micros: u64,
    pub total_micros: u64,
    pub entries: usize,
    pub resident_rows: usize,
    pub resident_bytes: usize,
    pub max_bytes: usize,
}

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

/// The two-layer cache behind one key.
#[derive(Debug)]
pub struct StrataCache {
    entries: HashMap<String, StoredEntry>,
    max_bytes: usize,
    resident_bytes: usize,
    clock: u64,
    disk: Option<DiskStore>,
}

impl StrataCache {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_bytes,
            resident_bytes: 0,
            clock: 0,
            disk: None,
        }
    }

    pub fn with_disk(mut self, disk: DiskStore) -> Self {
        self.disk = Some(disk);
        self
    }

    pub fn from_args(args: &crate::arg::Args) -> Self {
        let cache = Self::new(args.cache_max_bytes());
        match args.cache_dir() {
            Some(directory) => {
                cache.with_disk(DiskStore::new(directory.to_path_buf(), args.cache_disk_max_bytes()))
            }
            None => cache,
        }
    }

    pub fn state_stats(&self) -> CacheStateStats {
        CacheStateStats {
            entries: self.entries.len(),
            resident_rows: self
                .entries
                .values()
                .map(|stored| stored.entry.row_count())
                .sum(),
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
                stored.last_used = self.clock;
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
    pub fn insert(&mut self, key: String, entry: Arc<CacheEntry>) {
        if let Some(disk) = self.disk.as_mut() {
            disk.put(&key, &entry);
        }
        self.retain(key, entry);
    }

    fn retain(&mut self, key: String, entry: Arc<CacheEntry>) {
        if self.max_bytes == 0 || entry.bytes > self.max_bytes {
            return;
        }
        self.clock = self.clock.wrapping_add(1);
        if let Some(previous) = self.entries.remove(&key) {
            self.resident_bytes = self.resident_bytes.saturating_sub(previous.entry.bytes);
        }
        self.resident_bytes += entry.bytes;
        self.entries.insert(
            key,
            StoredEntry {
                entry,
                last_used: self.clock,
            },
        );
        while self.resident_bytes > self.max_bytes {
            let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, stored)| stored.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest_key) {
                self.resident_bytes = self.resident_bytes.saturating_sub(removed.entry.bytes);
            }
        }
    }
}

// -------------------------------------------------------------------- disk

/// A directory of states, one file per key, shared by every process that
/// points at it.  Writes land whole (temporary file, then rename) and reads
/// verify the digest each relation was written with, so a peer's half-written
/// or damaged file is a miss and never a wrong answer.
#[derive(Debug)]
pub struct DiskStore {
    root: PathBuf,
    max_bytes: u64,
    writes_since_sweep: usize,
}

impl DiskStore {
    pub fn new(root: PathBuf, max_bytes: u64) -> Self {
        Self {
            root,
            max_bytes,
            writes_since_sweep: 0,
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

    fn put(&mut self, key: &str, entry: &CacheEntry) {
        let path = self.path(key);
        if let Err(error) = self.write(&path, entry) {
            tracing::warn!("state store could not write {}: {error}", path.display());
            return;
        }
        self.writes_since_sweep += 1;
        if self.writes_since_sweep >= DISK_SWEEP_EVERY {
            self.writes_since_sweep = 0;
            self.sweep();
        }
    }

    fn write(&self, path: &Path, entry: &CacheEntry) -> io::Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "state path has no parent"))?;
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

    /// Drop the least recently read states until the store fits its budget.
    fn sweep(&self) {
        let mut files = Vec::new();
        let mut total = 0u64;
        let Ok(shards) = fs::read_dir(&self.root) else {
            return;
        };
        for shard in shards.flatten() {
            let Ok(entries) = fs::read_dir(shard.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|extension| extension.to_str()) != Some("state") {
                    continue;
                }
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                let used = metadata
                    .accessed()
                    .or_else(|_| metadata.modified())
                    .unwrap_or(UNIX_EPOCH);
                total += metadata.len();
                files.push((used, metadata.len(), path));
            }
        }
        if total <= self.max_bytes {
            return;
        }
        files.sort();
        for (_, size, path) in files {
            if total <= self.max_bytes {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
            }
        }
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
    }

    let mut reader = Reader { bytes, at: 0 };
    if reader.take(STATE_MAGIC.len())? != STATE_MAGIC {
        return None;
    }
    let count = reader.u32()? as usize;
    let mut relations = BTreeMap::new();
    for _ in 0..count {
        let name_len = reader.u32()? as usize;
        let name = std::str::from_utf8(reader.take(name_len)?).ok()?.to_string();
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
    if reader.at != bytes.len() {
        return None;
    }
    Some(CacheEntry::new(relations))
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
        Arc::new(RelationState::new(name, rows.first().map_or(1, Vec::len), rows))
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
        cache.insert("key".to_string(), Arc::new(CacheEntry::new(BTreeMap::new())));
        assert_eq!(cache.state_stats().entries, 0);
    }

    #[test]
    fn a_disk_entry_round_trips_and_a_damaged_one_is_a_miss() {
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
        let entry = CacheEntry::new(relations);
        let key = "ab".repeat(32);
        store.put(&key, &entry);

        let read = store.get(&key).expect("stored entry reads back");
        assert_eq!(read.relations["H"].rows, entry.relations["H"].rows);
        assert_eq!(read.relations["K"].digest, entry.relations["K"].digest);

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

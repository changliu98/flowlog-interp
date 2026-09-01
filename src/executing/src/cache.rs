//! Process-resident materialized stratum cache.
//!
//! A Differential Dataflow graph has fixed topology: arbitrary edits cannot be
//! spliced into an already assembled recursive scope. Instead, the daemon starts
//! a fresh graph for each reload and injects the complete output of every cached
//! stratum as an input collection. A stratum key binds its rules to the exact
//! versions of the relations it reads, so a rule edit invalidates that stratum
//! and its dependants without invalidating unrelated work.

use parsing::parser::Program;
use parsing::rule::Predicate;
use parsing::Val;
use planning::strata::GroupStrataQueryPlan;
use reading::inspect::MaterializedUpdates;
use reading::SEMIRING_TYPE;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const CACHE_ABI: &str = "flowlog-interp/materialized-strata/v1";

#[derive(Debug, Clone)]
pub struct CacheContext<'a> {
    pub facts: &'a Path,
    pub delimiter: u8,
    pub fat_mode: bool,
    pub opt_level: Option<u8>,
}

#[derive(Debug, Clone)]
pub struct RelationSnapshot {
    pub name: String,
    pub arity: usize,
    pub rows: Arc<Vec<Vec<Val>>>,
}

impl RelationSnapshot {
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

#[derive(Debug)]
pub struct CacheEntry {
    pub relations: BTreeMap<String, RelationSnapshot>,
    bytes: usize,
}

impl CacheEntry {
    fn new(relations: BTreeMap<String, RelationSnapshot>) -> Self {
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
}

#[derive(Debug, Clone)]
pub struct PreparedGroup {
    pub key: String,
    pub hit: Option<Arc<CacheEntry>>,
    pub captures: BTreeMap<String, MaterializedUpdates>,
    arities: BTreeMap<String, usize>,
}

#[derive(Debug, Clone)]
pub struct PreparedCache {
    pub groups: Vec<PreparedGroup>,
    input_files: Vec<InputFileGuard>,
    hits: usize,
    misses: usize,
    rows_loaded: usize,
}

#[derive(Debug, Clone)]
struct InputFileGuard {
    path: PathBuf,
    digest: String,
}

impl PreparedCache {
    pub fn verify_inputs(&self) -> io::Result<()> {
        for input in &self.input_files {
            let observed = file_digest(&input.path)?;
            if observed != input.digest {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "input relation {} changed while the dataflow was executing; refusing to cache a mixed snapshot",
                        input.path.display()
                    ),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CacheRunStats {
    pub hits: usize,
    pub misses: usize,
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

#[derive(Debug)]
struct StoredEntry {
    entry: Arc<CacheEntry>,
    last_used: u64,
}

#[derive(Debug)]
pub struct StrataCache {
    entries: HashMap<String, StoredEntry>,
    max_bytes: usize,
    resident_bytes: usize,
    clock: u64,
}

impl StrataCache {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_bytes,
            resident_bytes: 0,
            clock: 0,
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

    pub fn prepare(
        &mut self,
        program: &Program,
        groups: &[GroupStrataQueryPlan],
        context: CacheContext<'_>,
    ) -> io::Result<PreparedCache> {
        let base_key = program_base_key(program, &context);
        let (mut relation_versions, input_files) = edb_versions(program, &context)?;
        let mut prepared_groups = Vec::with_capacity(groups.len());
        let mut hits = 0;
        let mut misses = 0;
        let mut rows_loaded = 0;

        for group in groups {
            let head_arities = group
                .heads()
                .into_iter()
                // Non-recursive SIP expansion is sliced into cascading groups;
                // those private heads are real boundaries needed by the next
                // slice. Recursive SIP heads stay inside one iterative scope and
                // cannot be materialized after `leave`, so only its public heads
                // are cached.
                .filter(|(name, _)| !group.is_recursive() || !name.contains("_sip"))
                .collect::<BTreeMap<_, _>>();
            let internal_heads = head_arities.keys().cloned().collect::<BTreeSet<_>>();
            let mut inputs = BTreeMap::new();

            for rule in group.rules() {
                for predicate in rule.rhs() {
                    let name = match predicate {
                        Predicate::AtomPredicate(atom) | Predicate::NegatedAtomPredicate(atom) => {
                            atom.name()
                        }
                        Predicate::ComparePredicate(_) | Predicate::CallPredicate(_) => continue,
                    };

                    if let Some(version) = relation_versions.get(name) {
                        inputs.insert(name.to_string(), version.clone());
                    } else if !internal_heads.contains(name) {
                        // Validation normally makes this unreachable. Keeping the
                        // missing name in the key makes a future relaxed frontend
                        // fail closed instead of aliasing another cache entry.
                        inputs.insert(name.to_string(), "<missing>".to_string());
                    }
                }
            }

            // A later rule group may add contributions to a relation that already
            // exists. Its old value is an implicit input to the collector.
            for head in &internal_heads {
                if let Some(version) = relation_versions.get(head) {
                    inputs.insert(head.clone(), version.clone());
                }
            }

            let key = group_key(&base_key, group, &inputs);
            let hit = self.lookup(&key).filter(|entry| {
                head_arities.iter().all(|(name, arity)| {
                    entry
                        .relations
                        .get(name)
                        .is_some_and(|relation| relation.arity == *arity)
                }) && entry.relations.len() == head_arities.len()
            });

            let captures = if let Some(entry) = &hit {
                hits += 1;
                rows_loaded += entry.row_count();
                BTreeMap::new()
            } else {
                misses += 1;
                head_arities
                    .keys()
                    .map(|name| {
                        (
                            name.clone(),
                            Arc::new(Mutex::new(Vec::<(Vec<Val>, isize)>::new())),
                        )
                    })
                    .collect()
            };

            for head in head_arities.keys() {
                relation_versions.insert(head.clone(), relation_key(&key, head));
            }

            prepared_groups.push(PreparedGroup {
                key,
                hit,
                captures,
                arities: head_arities,
            });
        }

        Ok(PreparedCache {
            groups: prepared_groups,
            input_files,
            hits,
            misses,
            rows_loaded,
        })
    }

    pub fn commit(&mut self, prepared: &PreparedCache) -> CacheRunStats {
        let mut rows_cached = 0;

        for group in &prepared.groups {
            if group.hit.is_some() {
                continue;
            }

            let mut relations = BTreeMap::new();
            for (name, updates) in &group.captures {
                let mut consolidated = BTreeMap::<Vec<Val>, isize>::new();
                let mut updates = updates.lock().expect("materialized relation lock poisoned");
                for (row, difference) in updates.drain(..) {
                    *consolidated.entry(row).or_default() += difference;
                }
                let rows = consolidated
                    .into_iter()
                    .filter_map(|(row, difference)| (difference > 0).then_some(row))
                    .collect::<Vec<_>>();
                rows_cached += rows.len();
                relations.insert(
                    name.clone(),
                    RelationSnapshot {
                        name: name.clone(),
                        arity: group.arities[name],
                        rows: Arc::new(rows),
                    },
                );
            }
            self.insert(group.key.clone(), Arc::new(CacheEntry::new(relations)));
        }

        let state = self.state_stats();
        CacheRunStats {
            hits: prepared.hits,
            misses: prepared.misses,
            rows_loaded: prepared.rows_loaded,
            rows_cached,
            planning_micros: 0,
            execution_micros: 0,
            total_micros: 0,
            entries: state.entries,
            resident_rows: state.resident_rows,
            resident_bytes: state.resident_bytes,
            max_bytes: state.max_bytes,
        }
    }

    fn lookup(&mut self, key: &str) -> Option<Arc<CacheEntry>> {
        self.clock = self.clock.wrapping_add(1);
        let stored = self.entries.get_mut(key)?;
        stored.last_used = self.clock;
        Some(Arc::clone(&stored.entry))
    }

    fn insert(&mut self, key: String, entry: Arc<CacheEntry>) {
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

fn program_base_key(program: &Program, context: &CacheContext<'_>) -> String {
    let mut hasher = KeyHasher::new();
    hasher.add(CACHE_ABI);
    hasher.add(SEMIRING_TYPE);
    hasher.add(&context.delimiter.to_string());
    hasher.add(if context.fat_mode { "fat" } else { "thin" });
    hasher.add(&format!("opt={:?}", context.opt_level));

    let mut declarations = program
        .edbs()
        .iter()
        .map(|decl| format!("edb:{decl:?}"))
        .chain(program.idbs().iter().map(|decl| format!("idb:{decl:?}")))
        .collect::<Vec<_>>();
    declarations.sort();
    for declaration in declarations {
        hasher.add(&declaration);
    }
    if let Some(embedded) = program.embedded_rust() {
        hasher.add("embedded-rust");
        hasher.add(embedded.source());
    }
    hasher.finish()
}

fn edb_versions(
    program: &Program,
    context: &CacheContext<'_>,
) -> io::Result<(HashMap<String, String>, Vec<InputFileGuard>)> {
    let mut versions = HashMap::new();
    let mut input_files = Vec::new();
    for declaration in program.edbs() {
        let path = declaration
            .path()
            .map(|path| context.facts.join(path))
            .unwrap_or_else(|| context.facts.join(format!("{}.facts", declaration.name())));
        let digest = file_digest(&path)?;
        let mut hasher = KeyHasher::new();
        hasher.add("edb");
        hasher.add(&format!("{declaration:?}"));
        hasher.add(&digest);
        versions.insert(declaration.name().to_string(), hasher.finish());
        input_files.push(InputFileGuard { path, digest });
    }
    Ok((versions, input_files))
}

fn group_key(
    base_key: &str,
    group: &GroupStrataQueryPlan,
    inputs: &BTreeMap<String, String>,
) -> String {
    let mut hasher = KeyHasher::new();
    hasher.add("group");
    hasher.add(base_key);
    hasher.add(if group.is_recursive() {
        "recursive"
    } else {
        "non-recursive"
    });

    let mut rules = group
        .rules()
        .iter()
        // Debug is the parser AST here, not a user-facing diagnostic: unlike
        // Display it retains predicate variants and optimizer annotations.
        .map(|rule| format!("{rule:?}"))
        .collect::<Vec<_>>();
    rules.sort();
    for rule in rules {
        hasher.add(&rule);
    }
    for (name, version) in inputs {
        hasher.add(name);
        hasher.add(version);
    }
    hasher.finish()
}

fn relation_key(group_key: &str, name: &str) -> String {
    let mut hasher = KeyHasher::new();
    hasher.add("relation");
    hasher.add(group_key);
    hasher.add(name);
    hasher.finish()
}

fn file_digest(path: &Path) -> io::Result<String> {
    let mut file = File::open(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot hash input relation {}: {error}", path.display()),
        )
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

struct KeyHasher(Sha256);

impl KeyHasher {
    fn new() -> Self {
        Self(Sha256::new())
    }

    fn add(&mut self, value: &str) {
        self.0.update((value.len() as u64).to_le_bytes());
        self.0.update(value.as_bytes());
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
}

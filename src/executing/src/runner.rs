//! One evaluation: strata onto dataflows, dataflows onto the evaluation's
//! worker set, states out.
//!
//! Two schedules exist. *Per stratum* reads along the state cache: every unit
//! of a stratum is keyed from settled states, served when the cache holds it,
//! and otherwise assembled - as rule contributions where the unit is an
//! ordinary non-recursive head, whole otherwise - into one dataflow for the
//! stratum. *Whole program* builds every stratum into one dataflow, sharing
//! intermediates across strata, and never touches the cache. Both run on the
//! same worker set and capture the same states.

use crate::accounting::{Budget, MemoryCounter};
use crate::cache::{
    contribution_key, symbol_cells, unit_inputs, unit_key, units_of_stratum, CacheEntry,
    CacheRunStats, HitSource, RelationState, StrataCache, Unit,
};
use crate::canonical::canonical_rule;
use crate::dataflow::{Assembly, Computation};
use crate::engine::{budget_of, Engine, EngineConfig, EvaluationOptions, Schedule};
use crate::native_calls::NativeCallModule;
use crate::rows::Rows;
use crate::worker::WorkerSet;
use catalog::head::aggregation_catalog_from_program;
use itertools::Itertools;
use parsing::diagnostic::Result;
use parsing::parser::Program;
use planning::program::{plan_rules, ProgramQueryPlan};
use reading::inspect::{MaterializedUpdates, UpdateBatch};
use reading::{KV_MAX, ROW_MAX};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use strata::stratification::Strata;
use tracing::{info, warn};

/// The cache behind its lock, poisoned or not: a run that panicked mid-way
/// wrote nothing partial, so the entries are as good as they were.
pub fn lock(cache: &Mutex<StrataCache>) -> MutexGuard<'_, StrataCache> {
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What one evaluation leaves behind for the caller and the explainer.
pub struct RunOutcome {
    pub program: Program,
    pub states: HashMap<String, Arc<RelationState>>,
    pub stats: CacheRunStats,
    pub budget: Arc<Budget>,
    pub native: Option<Arc<NativeCallModule>>,
}

/// Plan the whole program, for the fat-mode decision and for `check`.
pub(crate) fn plan_check(strata: &Strata, config: &EngineConfig) -> Result<(ProgramQueryPlan, bool)> {
    let plan = ProgramQueryPlan::from_strata(strata, !config.sharing, config.opt_level);
    debugging::debugger::display_info("Program Query Plans", true, format!("{}", plan));
    let fat_mode = plan.should_use_fat_mode(config.fat_mode, KV_MAX, ROW_MAX);
    if fat_mode && !config.fat_mode {
        warn!("WARNING: Fat mode automatically enabled due to high arity");
        warn!(
            "         Maximal incomparable arity pairs found: {:?}",
            plan.maximal_arity_pairs()
        );
    }
    Ok((plan, fat_mode))
}

fn native_module(engine: &Engine, program: &Program) -> Result<Option<Arc<NativeCallModule>>> {
    program
        .embedded_rust()
        .map(|embedded| {
            NativeCallModule::compile_and_load(
                embedded,
                engine.config().call_cache.as_deref(),
                Arc::clone(engine.symbols()),
                program.name(),
            )
        })
        .transpose()
}

/// One cache miss to evaluate. A contribution belongs to a pending head union;
/// a whole-unit result can be published directly at the stratum boundary.
struct PendingComputation<'a> {
    unit: Unit<'a>,
    key: String,
    inputs: BTreeMap<String, Arc<RelationState>>,
    contribution_to: Option<usize>,
}

struct PendingUnion {
    head: String,
    arity: usize,
    key: String,
    inherited: Option<Arc<RelationState>>,
    contributions: Vec<Arc<RelationState>>,
}

/// Run one program over its input states.
pub fn run(
    engine: &Engine,
    program: Program,
    states: HashMap<String, Arc<RelationState>>,
    options: &EvaluationOptions,
) -> Result<RunOutcome> {
    let total_started = Instant::now();
    let profile_run = crate::profile::ProfileRun::start(options.profile_path.as_ref());
    let profile = profile_run.as_ref().map(|run| Arc::clone(&run.profile));
    let config = engine.config();
    let strata = Strata::try_from_parser(program.clone())?;
    debugging::debugger::display_info("Strata", false, format!("{}\n{}", strata.dependency_graph(), strata));
    let (whole_plan, fat_mode) = plan_check(&strata, config)?;
    if let Some(profile) = &profile { profile.phase("compiling_helpers"); }
    let native = native_module(engine, &program)?;
    let idb_map = Arc::new(aggregation_catalog_from_program(&program));
    let budget = budget_of(options);
    let _tag = crate::accounting::MemoryTag::new(budget.memory());
    let workers = WorkerSet::start(config.workers, Arc::clone(&budget), profile.clone())?;
    if let Some(profile) = &profile { profile.phase("executing"); }
    let planning_elapsed = total_started.elapsed();

    let mut stats = CacheRunStats::default();
    stats.planning_micros = duration_micros(planning_elapsed);

    let states = match options.schedule(config) {
        Schedule::WholeProgram => run_whole(
            &program,
            &strata,
            whole_plan,
            states,
            &workers,
            fat_mode,
            &idb_map,
            &native,
            &budget,
            &mut stats,
        )?,
        Schedule::PerStratum => run_per_stratum(
            engine,
            &program,
            &strata,
            states,
            &workers,
            fat_mode,
            &idb_map,
            &native,
            &budget,
            options.cached(config),
            &mut stats,
        )?,
    };
    drop(workers);

    let cache_state = engine.cache_stats();
    stats.entries = cache_state.entries;
    stats.resident_rows = cache_state.resident_rows;
    stats.resident_bytes = cache_state.resident_bytes;
    stats.max_bytes = cache_state.max_bytes;
    stats.peak_memory_bytes = budget.memory().peak();
    stats.materialized_rows = budget.tuples();
    stats.total_micros = duration_micros(total_started.elapsed());
    info!(
        "state cache: {} strata, {} units, {} hits ({} from disk, {} after a miss), {} misses; contributions: {} hits ({} from disk), {} misses; {} rules evaluated",
        stats.strata, stats.units, stats.hits, stats.disk_hits, stats.hits_after_miss, stats.misses,
        stats.contribution_hits, stats.contribution_disk_hits, stats.contribution_misses,
        stats.rules_evaluated
    );
    if let Some(profile) = &profile { profile.complete(); }
    Ok(RunOutcome {
        program,
        states,
        stats,
        budget,
        native,
    })
}

/// Every stratum in one dataflow, every head captured at the end.
fn run_whole(
    program: &Program,
    strata: &Strata,
    plan: ProgramQueryPlan,
    mut states: HashMap<String, Arc<RelationState>>,
    workers: &WorkerSet,
    fat_mode: bool,
    idb_map: &Arc<HashMap<String, catalog::head::AggregationHeadIDB>>,
    native: &Option<Arc<NativeCallModule>>,
    budget: &Arc<Budget>,
    stats: &mut CacheRunStats,
) -> Result<HashMap<String, Arc<RelationState>>> {
    let started = Instant::now();
    stats.strata = strata.strata().len();
    stats.units = 1;
    stats.misses = 1;
    stats.rules_evaluated = program.rules().len();

    let mut heads: BTreeMap<String, usize> = BTreeMap::new();
    for rule in program.rules() {
        if !rule.is_sideways() {
            heads.insert(rule.head().name().clone(), rule.head().arity());
        }
    }
    // Every input, and every declared relation a rule reads but none
    // defines: it is empty, as the per-stratum schedule has it.
    let read: HashSet<&str> = program
        .rules()
        .iter()
        .flat_map(|rule| rule.rhs())
        .filter_map(|predicate| match predicate {
            parsing::rule::Predicate::AtomPredicate(atom)
            | parsing::rule::Predicate::NegatedAtomPredicate(atom) => Some(atom.name()),
            _ => None,
        })
        .collect();
    let inputs = program
        .edbs()
        .iter()
        .chain(program.idbs().iter().filter(|declaration| {
            !heads.contains_key(declaration.name()) && read.contains(declaration.name())
        }))
        .map(|declaration| {
            let state = states
                .get(declaration.name())
                .cloned()
                .unwrap_or_else(|| Arc::new(RelationState::empty(declaration.name(), declaration.arity())));
            (declaration.name().to_string(), state)
        })
        .collect::<BTreeMap<_, _>>();
    let captures: BTreeMap<String, MaterializedUpdates> = heads
        .keys()
        .map(|head| (head.clone(), Arc::new(Mutex::new(Vec::new()))))
        .collect();
    let assembly = Arc::new(Assembly {
        computations: vec![Computation {
            groups: plan.program_plan().clone(),
            inputs,
            captures: captures.clone(),
        }],
        fat_mode,
        idb_map: Arc::clone(idb_map),
        native_calls: native.clone(),
        budget: Arc::clone(budget),
        program_name: program.name().to_string(),
        profile: workers.profile(),
    });
    workers.run(assembly)?;
    stats.execution_micros = duration_micros(started.elapsed());

    // Each head settles on its own: merging and digesting are independent.
    let settled = parallel_map(captures.into_iter().collect(), workers.peers(), budget,
        |(name, updates): (String, MaterializedUpdates)| {
            let arity = heads[&name];
            let rows = consolidate(&updates, arity, budget.memory());
            budget.note_tuples(rows.len() as u64);
            let state = Arc::new(RelationState::from_sorted(&name, arity, rows));
            (name, state)
        });
    for (name, state) in settled {
        stats.rows_cached += state.rows.len();
        states.insert(name, state);
    }
    budget.poll()?;
    Ok(states)
}

/// Stratum by stratum, reading along the cache.
fn run_per_stratum(
    engine: &Engine,
    program: &Program,
    strata: &Strata,
    mut states: HashMap<String, Arc<RelationState>>,
    workers: &WorkerSet,
    fat_mode: bool,
    idb_map: &Arc<HashMap<String, catalog::head::AggregationHeadIDB>>,
    native: &Option<Arc<NativeCallModule>>,
    budget: &Arc<Budget>,
    cached: bool,
    stats: &mut CacheRunStats,
) -> Result<HashMap<String, Arc<RelationState>>> {
    let config = engine.config();
    let cache = engine.cache();
    let symbols = engine.symbols();
    let mut execution = Duration::ZERO;
    let mut cache_time = Duration::ZERO;
    let strata_rules = strata.strata();
    let recursive = strata.is_recursive_strata_bitmap();
    stats.strata = strata_rules.len();

    // Every lookup of a stratum at once: the memory tier under one lock, then
    // the disk tier's reads and decodes on several threads without it.
    let lookup_all = |requests: &[(&str, &BTreeMap<String, usize>)]| -> Vec<Option<(Arc<CacheEntry>, HitSource)>> {
        if !cached {
            return vec![None; requests.len()];
        }
        let (mut found, disk) = {
            let mut cache = lock(cache);
            let found: Vec<Option<(Arc<CacheEntry>, HitSource)>> = requests
                .iter()
                .map(|(key, heads)| cache.lookup_memory(key, heads).map(|entry| (entry, HitSource::Memory)))
                .collect();
            (found, cache.disk())
        };
        if let Some(disk) = disk {
            // One batch: the store reads the hits sorted by where they lie.
            let wanted: Vec<usize> = (0..requests.len()).filter(|&index| found[index].is_none()).collect();
            let batch: Vec<(&str, &BTreeMap<String, usize>)> = wanted.iter().map(|&index| requests[index]).collect();
            let loaded = disk.load_many(&batch, workers.peers());
            let mut cache = lock(cache);
            for (index, entry) in wanted.into_iter().zip(loaded) {
                if let Some(entry) = entry {
                    let entry = Arc::new(entry);
                    cache.adopt(requests[index].0.to_string(), Arc::clone(&entry));
                    found[index] = Some((entry, HitSource::Disk));
                }
            }
        }
        for (entry, _) in found.iter().flatten() {
            // A cached state may hold symbols this engine never interned.
            let _ = symbols.absorb(entry.symbols.iter().cloned());
        }
        found
    };
    let store = |key: String, entry: Arc<CacheEntry>, contribution: bool| {
        if cached {
            if contribution {
                lock(cache).insert_contribution(key, entry);
            } else {
                lock(cache).insert(key, entry);
            }
        }
    };
    let disk = if cached { lock(cache).disk() } else { None };
    let counters_before = disk.as_ref().map(|disk| disk.counters()).unwrap_or_default();
    let entry_of = |relations: BTreeMap<String, Arc<RelationState>>| -> Arc<CacheEntry> {
        let mut cells = Vec::new();
        for (name, state) in &relations {
            if let Some(types) = program.column_types(name) {
                cells.extend(symbol_cells(&state.rows, types));
            }
        }
        let texts = if cells.is_empty() {
            Vec::new()
        } else {
            symbols.texts_of(cells)
        };
        Arc::new(CacheEntry::with_symbols(relations, texts))
    };

    for (stratum, &is_recursive) in strata_rules.iter().zip(recursive.iter()) {
        budget.poll()?;
        let cache_started = Instant::now();
        let execution_before = execution;
        let units = units_of_stratum(stratum, is_recursive);
        stats.units += units.len();
        let mut settled = Vec::new();
        let mut missed = Vec::new();
        let mut unions: Vec<PendingUnion> = Vec::new();
        let mut contributions_to_store: Vec<(String, Arc<CacheEntry>)> = Vec::new();
        // Keys read only earlier strata's states, so a stratum's units are
        // keyed independently of each other.
        let keyed = parallel_map(units, workers.peers(), budget, |unit: Unit<'_>| {
            let inputs = unit_inputs(&unit, &states, program);
            let key = unit_key(&unit, &inputs, program);
            (unit, inputs, key)
        });
        let found = lookup_all(
            &keyed
                .iter()
                .map(|(unit, _, key)| (key.as_str(), &unit.heads))
                .collect::<Vec<_>>(),
        );
        // A contribution to look up: its union and its rule as a unit.
        let mut contribution_requests: Vec<(usize, Unit<'_>)> = Vec::new();
        for ((unit, inputs, key), found) in keyed.into_iter().zip(found) {
            match found {
                Some((entry, source)) => {
                    stats.hits += 1;
                    if source == HitSource::Disk {
                        stats.disk_hits += 1;
                    }
                    if stats.misses > 0 {
                        stats.hits_after_miss += 1;
                    }
                    stats.rows_loaded += entry.row_count();
                    settled.push(entry);
                }
                None => {
                    stats.misses += 1;
                    if unit.recursive || unit.heads.keys().any(|head| idb_map.contains_key(head)) {
                        missed.push(PendingComputation {
                            unit,
                            key,
                            inputs,
                            contribution_to: None,
                        });
                        continue;
                    }

                    // Every ordinary non-recursive unit has one head. Its
                    // inherited state supports the final union, but must not
                    // enter a rule contribution or that contribution's key.
                    let (head, &arity) = unit.heads.first_key_value().expect("unit has a head");
                    let pending = PendingUnion {
                        head: head.clone(),
                        arity,
                        key,
                        inherited: inputs.get(head).cloned(),
                        contributions: Vec::new(),
                    };
                    let mut distinct_rules = HashSet::new();
                    for rule in unit.rules {
                        if !distinct_rules.insert(canonical_rule(rule)) {
                            continue;
                        }
                        let contribution = Unit {
                            heads: unit.heads.clone(),
                            rules: vec![rule],
                            recursive: false,
                        };
                        contribution_requests.push((unions.len(), contribution));
                    }
                    unions.push(pending);
                }
            }
        }

        // Every missed unit's rule contributions, keyed and looked up at once.
        let keyed_contributions = parallel_map(
            contribution_requests,
            workers.peers(),
            budget,
            |(union, contribution): (usize, Unit<'_>)| {
                let head = contribution.heads.keys().next().expect("unit has a head").clone();
                let mut body_inputs = unit_inputs(&contribution, &states, program);
                body_inputs.remove(&head);
                let key = contribution_key(&contribution, &body_inputs, program);
                (union, head, contribution, body_inputs, key)
            },
        );
        let found = lookup_all(
            &keyed_contributions
                .iter()
                .map(|(_, _, contribution, _, key)| (key.as_str(), &contribution.heads))
                .collect::<Vec<_>>(),
        );
        for ((union, head, contribution, body_inputs, key), found) in keyed_contributions.into_iter().zip(found) {
            match found {
                Some((entry, source)) => {
                    stats.contribution_hits += 1;
                    if source == HitSource::Disk {
                        stats.contribution_disk_hits += 1;
                    }
                    stats.contribution_rows_loaded += entry.row_count();
                    unions[union].contributions.push(Arc::clone(&entry.relations[&head]));
                }
                None => {
                    stats.contribution_misses += 1;
                    missed.push(PendingComputation {
                        unit: contribution,
                        key,
                        inputs: body_inputs,
                        contribution_to: Some(union),
                    });
                }
            }
        }

        if !missed.is_empty() {
            let started = Instant::now();
            let mut computations = Vec::with_capacity(missed.len());
            let mut unit_captures: Vec<BTreeMap<String, MaterializedUpdates>> =
                Vec::with_capacity(missed.len());
            // Sideways slices are named by rule identifier; units assembled
            // into one dataflow must not reuse each other's. Intermediates
            // are shared within one computation and never across two, so the
            // seen set starts afresh for each.
            let mut first_identifiers = Vec::with_capacity(missed.len());
            let mut next_rule_identifier = 0usize;
            for pending in &missed {
                first_identifiers.push(next_rule_identifier);
                next_rule_identifier += pending.unit.rules.len();
                stats.rules_evaluated += pending.unit.rules.len();
            }
            // Planning one computation is independent of planning another.
            let planned = parallel_map(
                missed.iter().zip(first_identifiers).collect(),
                workers.peers(),
                budget,
                |(pending, first_identifier): (&PendingComputation<'_>, usize)| {
                    let unit = &pending.unit;
                    let mut seen_set = HashSet::new();
                    plan_rules(
                        &unit.rules,
                        unit.recursive,
                        config.opt_level,
                        program,
                        first_identifier,
                        &mut seen_set,
                        !config.sharing,
                    )
                },
            );
            for (pending, groups) in missed.iter().zip(planned) {
                let captures: BTreeMap<String, MaterializedUpdates> = pending
                    .unit
                    .heads
                    .keys()
                    .map(|head| (head.clone(), Arc::new(Mutex::new(Vec::new()))))
                    .collect();
                unit_captures.push(captures.clone());
                computations.push(Computation {
                    groups,
                    inputs: pending.inputs.clone(),
                    captures,
                });
            }
            workers.run(Arc::new(Assembly {
                computations,
                fat_mode,
                idb_map: Arc::clone(idb_map),
                native_calls: native.clone(),
                budget: Arc::clone(budget),
                program_name: program.name().to_string(),
                profile: workers.profile(),
            }))?;
            execution += started.elapsed();

            // Merging, sorting and digesting one unit's captures is
            // independent of every other unit's, and the workers are idle.
            let settled_units = parallel_map(
                missed.iter().zip(unit_captures).collect(),
                workers.peers(),
                budget,
                |(pending, captures): (&PendingComputation<'_>, BTreeMap<String, MaterializedUpdates>)| {
                    let mut relations = BTreeMap::new();
                    for (name, updates) in captures {
                        let arity = pending.unit.heads[&name];
                        let rows = consolidate(&updates, arity, budget.memory());
                        budget.note_tuples(rows.len() as u64);
                        relations.insert(
                            name.clone(),
                            Arc::new(RelationState::from_sorted(&name, arity, rows)),
                        );
                    }
                    entry_of(relations)
                },
            );
            for (pending, entry) in missed.into_iter().zip(settled_units) {
                if let Some(index) = pending.contribution_to {
                    stats.contribution_rows_cached += entry.row_count();
                    let union = &mut unions[index];
                    union
                        .contributions
                        .push(Arc::clone(&entry.relations[&union.head]));
                    contributions_to_store.push((pending.key, entry));
                } else {
                    stats.rows_cached += entry.row_count();
                    settled.push(Arc::clone(&entry));
                    store(pending.key, entry, false);
                }
            }
        }

        // Reconstruct each head from its active contributions, one head per
        // thread. A deletion omits one support, never subtracts a tuple still
        // supported by another rule or by the head's state entering this stratum.
        let union_entries = parallel_map(unions, workers.peers(), budget, |union: PendingUnion| {
            let contributions: Vec<_> = union
                .inherited
                .iter()
                .chain(union.contributions.iter())
                .collect();
            let state = if contributions.len() == 1 {
                Arc::clone(contributions[0])
            } else {
                let total = contributions.iter().map(|state| state.rows.len()).sum();
                let mut rows = Rows::with_capacity(union.arity, total);
                for row in contributions.iter().map(|state| state.rows.iter()).kmerge().dedup() {
                    rows.push(row);
                }
                Arc::new(RelationState::from_sorted(&union.head, union.arity, rows))
            };
            (union.key, entry_of(BTreeMap::from([(union.head, state)])))
        });
        for (key, entry) in union_entries {
            stats.rows_cached += entry.row_count();
            store(key, Arc::clone(&entry), false);
            settled.push(entry);
        }
        // Contributions after the units: one that is its head's only support
        // holds the union's rows, which are then stored with the units already.
        for (key, entry) in contributions_to_store {
            store(key, entry, true);
        }

        for entry in settled {
            for (name, state) in &entry.relations {
                states.insert(name.clone(), Arc::clone(state));
            }
        }
        // What this stratum stored, on disk for the processes that follow.
        if let Some(disk) = &disk {
            disk.flush();
        }
        cache_time += cache_started
            .elapsed()
            .saturating_sub(execution.saturating_sub(execution_before));
        budget.poll()?;
    }

    if let Some(disk) = &disk {
        let started = Instant::now();
        disk.flush();
        stats.disk += disk.maintain();
        let counters = disk.counters();
        stats.disk_reads = counters.read_requests - counters_before.read_requests;
        stats.disk_read_bytes = counters.read_bytes - counters_before.read_bytes;
        stats.disk_written_bytes = counters.written_bytes - counters_before.written_bytes;
        stats.disk_shared_states = counters.reused_blobs - counters_before.reused_blobs;
        cache_time += started.elapsed();
    }
    stats.execution_micros = duration_micros(execution);
    stats.cache_micros = duration_micros(cache_time);
    Ok(states)
}

/// `function` over every item on up to `threads` threads, results in item
/// order. Each helper thread charges its allocations to the evaluation.
fn parallel_map<T: Send, R: Send>(
    items: Vec<T>,
    threads: usize,
    budget: &Arc<Budget>,
    function: impl Fn(T) -> R + Sync,
) -> Vec<R> {
    let threads = threads.min(items.len());
    if threads <= 1 {
        return items.into_iter().map(function).collect();
    }
    let count = items.len();
    let items: Vec<Mutex<Option<T>>> = items.into_iter().map(|item| Mutex::new(Some(item))).collect();
    let results: Vec<Mutex<Option<R>>> = (0..count).map(|_| Mutex::new(None)).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let _tag = crate::accounting::MemoryTag::new(budget.memory());
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if index >= count {
                        break;
                    }
                    let item = items[index].lock().unwrap_or_else(|p| p.into_inner()).take();
                    if let Some(item) = item {
                        let result = function(item);
                        *results[index].lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
                    }
                }
            });
        }
    });
    results
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap_or_else(|p| p.into_inner())
                .expect("every item was mapped")
        })
        .collect()
}

/// The rows a capture accumulated, as a set: every row whose total weight is
/// positive once the worker frontier has closed.
pub(crate) fn consolidate(updates: &MaterializedUpdates, arity: usize, _memory: &Arc<MemoryCounter>) -> Rows {
    let mut batches = std::mem::take(&mut *updates.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
    // Merge the workers' sorted batches pairwise; there is one per worker.
    while batches.len() > 1 {
        let mut merged = Vec::with_capacity(batches.len().div_ceil(2));
        let mut pending = batches.into_iter();
        while let Some(left) = pending.next() {
            merged.push(match pending.next() {
                Some(right) => merge_batches(left, right, arity),
                None => left,
            });
        }
        batches = merged;
    }
    let batch = batches.pop().unwrap_or_default();
    let mut rows = Rows::with_capacity(arity, batch.weights.len());
    for (index, &weight) in batch.weights.iter().enumerate() {
        if weight > 0 {
            rows.push(&batch.cells[index * arity..(index + 1) * arity]);
        }
    }
    rows
}

/// Two sorted batches as one, adding the weights of a row both hold and
/// dropping it when they cancel.
fn merge_batches(left: UpdateBatch, right: UpdateBatch, arity: usize) -> UpdateBatch {
    let row = |index: usize| index * arity..(index + 1) * arity;
    let (left_rows, right_rows) = (left.weights.len(), right.weights.len());
    let mut merged = UpdateBatch {
        cells: Vec::with_capacity(left.cells.len() + right.cells.len()),
        weights: Vec::with_capacity(left_rows + right_rows),
    };
    let (mut i, mut j) = (0, 0);
    while i < left_rows && j < right_rows {
        match left.cells[row(i)].cmp(&right.cells[row(j)]) {
            std::cmp::Ordering::Less => {
                merged.cells.extend_from_slice(&left.cells[row(i)]);
                merged.weights.push(left.weights[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                merged.cells.extend_from_slice(&right.cells[row(j)]);
                merged.weights.push(right.weights[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                let weight = left.weights[i] + right.weights[j];
                if weight != 0 {
                    merged.cells.extend_from_slice(&left.cells[row(i)]);
                    merged.weights.push(weight);
                }
                i += 1;
                j += 1;
            }
        }
    }
    merged.cells.extend_from_slice(&left.cells[i * arity..]);
    merged.weights.extend_from_slice(&left.weights[i..]);
    merged.cells.extend_from_slice(&right.cells[j * arity..]);
    merged.weights.extend_from_slice(&right.weights[j..]);
    merged
}

pub(crate) fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use parsing::Val;

    #[test]
    fn materialization_merges_signed_support_across_workers() {
        let batch = |rows: &[(Val, isize)]| UpdateBatch {
            cells: rows.iter().map(|(cell, _)| *cell).collect(),
            weights: rows.iter().map(|(_, weight)| *weight).collect(),
        };
        let updates = Arc::new(Mutex::new(vec![
            batch(&[(1, 2), (2, -1), (4, 3)]),
            batch(&[]),
            batch(&[(1, -2), (2, 2), (3, -1)]),
            batch(&[(3, 1), (4, -2), (5, -1)]),
        ]));
        let memory = Arc::new(MemoryCounter::default());
        assert_eq!(consolidate(&updates, 1, &memory), vec![vec![2], vec![4]]);
        assert!(consolidate(&updates, 1, &memory).is_empty());
        let empty_rows = Arc::new(Mutex::new(vec![
            UpdateBatch { cells: vec![], weights: vec![1] },
            UpdateBatch { cells: vec![], weights: vec![1] },
        ]));
        assert_eq!(consolidate(&empty_rows, 0, &memory).len(), 1);
    }
}

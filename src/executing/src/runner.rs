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
use crate::worker::WorkerSet;
use catalog::head::aggregation_catalog_from_program;
use itertools::Itertools;
use parsing::diagnostic::Result;
use parsing::parser::Program;
use parsing::Val;
use planning::program::{plan_rules, ProgramQueryPlan};
use reading::inspect::MaterializedUpdates;
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
    let config = engine.config();
    let strata = Strata::try_from_parser(program.clone())?;
    debugging::debugger::display_info("Strata", false, format!("{}\n{}", strata.dependency_graph(), strata));
    let (whole_plan, fat_mode) = plan_check(&strata, config)?;
    let native = native_module(engine, &program)?;
    let idb_map = Arc::new(aggregation_catalog_from_program(&program));
    let budget = budget_of(options);
    let _tag = crate::accounting::MemoryTag::new(budget.memory());
    let workers = WorkerSet::start(config.workers, Arc::clone(&budget))?;
    let planning_elapsed = total_started.elapsed();

    let mut stats = CacheRunStats::default();
    stats.planning_micros = duration_micros(planning_elapsed);

    let states = match options.schedule() {
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
            options.cached(),
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
    let inputs = program
        .edbs()
        .iter()
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
    });
    workers.run(assembly)?;
    stats.execution_micros = duration_micros(started.elapsed());

    for (name, updates) in captures {
        let rows = consolidate(&updates, budget.memory());
        budget.note_tuples(rows.len() as u64);
        let arity = heads[&name];
        stats.rows_cached += rows.len();
        states.insert(name.clone(), Arc::new(RelationState::from_sorted_rows(&name, arity, rows)));
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

    let lookup = |key: &str, heads: &BTreeMap<String, usize>| -> Option<(Arc<CacheEntry>, HitSource)> {
        if !cached {
            return None;
        }
        let found = lock(cache).lookup(key, heads);
        if let Some((entry, _)) = &found {
            // A cached state may hold symbols this engine never interned.
            let _ = symbols.absorb(entry.symbols.iter().cloned());
        }
        found
    };
    let store = |key: String, entry: Arc<CacheEntry>| -> crate::cache::DiskSweepStats {
        if !cached {
            return Default::default();
        }
        lock(cache).insert(key, entry)
    };
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
        for unit in units {
            let inputs = unit_inputs(&unit, &states, program);
            let key = unit_key(&unit, &inputs, program);
            match lookup(&key, &unit.heads) {
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
                    let mut pending = PendingUnion {
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
                        let mut body_inputs = unit_inputs(&contribution, &states, program);
                        body_inputs.remove(head);
                        let contribution_key = contribution_key(&contribution, &body_inputs, program);
                        match lookup(&contribution_key, &contribution.heads) {
                            Some((entry, source)) => {
                                stats.contribution_hits += 1;
                                if source == HitSource::Disk {
                                    stats.contribution_disk_hits += 1;
                                }
                                stats.contribution_rows_loaded += entry.row_count();
                                pending
                                    .contributions
                                    .push(Arc::clone(&entry.relations[head]));
                            }
                            None => {
                                stats.contribution_misses += 1;
                                missed.push(PendingComputation {
                                    unit: contribution,
                                    key: contribution_key,
                                    inputs: body_inputs,
                                    contribution_to: Some(unions.len()),
                                });
                            }
                        }
                    }
                    unions.push(pending);
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
            let mut next_rule_identifier = 0usize;
            for pending in &missed {
                let unit = &pending.unit;
                let mut seen_set = HashSet::new();
                let groups = plan_rules(
                    &unit.rules,
                    unit.recursive,
                    config.opt_level,
                    program,
                    next_rule_identifier,
                    &mut seen_set,
                    !config.sharing,
                );
                next_rule_identifier += unit.rules.len();
                stats.rules_evaluated += unit.rules.len();
                let captures: BTreeMap<String, MaterializedUpdates> = unit
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
            let assembly = Arc::new(Assembly {
                computations,
                fat_mode,
                idb_map: Arc::clone(idb_map),
                native_calls: native.clone(),
                budget: Arc::clone(budget),
                program_name: program.name().to_string(),
            });
            workers.run(assembly)?;
            execution += started.elapsed();

            for (pending, captures) in missed.into_iter().zip(unit_captures) {
                let mut relations = BTreeMap::new();
                for (name, updates) in captures {
                    let rows = consolidate(&updates, budget.memory());
                    budget.note_tuples(rows.len() as u64);
                    let arity = pending.unit.heads[&name];
                    relations.insert(
                        name.clone(),
                        Arc::new(RelationState::from_sorted_rows(&name, arity, rows)),
                    );
                }
                let entry = entry_of(relations);
                if let Some(index) = pending.contribution_to {
                    stats.contribution_rows_cached += entry.row_count();
                    let union = &mut unions[index];
                    union
                        .contributions
                        .push(Arc::clone(&entry.relations[&union.head]));
                } else {
                    stats.rows_cached += entry.row_count();
                    settled.push(Arc::clone(&entry));
                }
                stats.disk += store(pending.key, entry);
            }
        }

        for union in unions {
            // Reconstruct from active contributions. A deletion omits one
            // support, never subtracts a tuple still supported by another rule
            // or by the head's state entering this stratum.
            let contributions: Vec<_> = union
                .inherited
                .iter()
                .chain(union.contributions.iter())
                .collect();
            let state = if contributions.len() == 1 {
                Arc::clone(contributions[0])
            } else {
                let rows = contributions.iter().map(|state| state.rows.iter())
                    .kmerge().dedup().cloned().collect();
                Arc::new(RelationState::from_sorted_rows(&union.head, union.arity, rows))
            };
            stats.rows_cached += state.rows.len();
            let entry = entry_of(BTreeMap::from([(union.head, state)]));
            stats.disk += store(union.key, Arc::clone(&entry));
            settled.push(entry);
        }

        for entry in settled {
            for (name, state) in &entry.relations {
                states.insert(name.clone(), Arc::clone(state));
            }
        }
        cache_time += cache_started
            .elapsed()
            .saturating_sub(execution.saturating_sub(execution_before));
        budget.poll()?;
    }

    stats.execution_micros = duration_micros(execution);
    stats.cache_micros = duration_micros(cache_time);
    Ok(states)
}

/// The rows a capture accumulated, as a set: every row whose total weight is
/// positive once the worker frontier has closed.
pub(crate) fn consolidate(updates: &MaterializedUpdates, memory: &Arc<MemoryCounter>) -> Vec<Vec<Val>> {
    let batches = std::mem::take(&mut *updates.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
    let merged = crate::merge::sorted(batches,
        &|a: &(Vec<Val>, isize), b| a.0.cmp(&b.0),
        &|a, b| { a.1 += b.1; a.1 != 0 }, Some(memory));
    merged.into_iter().filter_map(|(row, weight)| (weight > 0).then_some(row)).collect()
}

pub(crate) fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialization_merges_signed_support_across_workers() {
        let updates = Arc::new(Mutex::new(vec![
            vec![(vec![], 1), (vec![1], 2), (vec![2], -1), (vec![4], 3)],
            vec![],
            vec![(vec![], -1), (vec![1], -2), (vec![2], 2), (vec![3], -1)],
            vec![(vec![3], 1), (vec![4], -2), (vec![5], -1)],
        ]));
        let memory = Arc::new(MemoryCounter::default());
        assert_eq!(consolidate(&updates, &memory), vec![vec![2], vec![4]]);
        assert!(consolidate(&updates, &memory).is_empty());
    }
}

//! One evaluation: the program onto one dataflow, the dataflow onto the
//! evaluation's worker set, states out.
//!
//! Every stratum is assembled into one dataflow, so intermediates are shared
//! across strata and nothing is materialized between them. The inputs enter
//! as sessions, every head is captured where the dataflow ends, and the
//! captures settle into sorted states once its frontier has closed.

use crate::accounting::{Budget, MemoryCounter};
use crate::dataflow::Assembly;
use crate::engine::{budget_of, Engine, EngineConfig, EvaluationOptions};
use crate::native_calls::NativeCallModule;
use crate::rows::Rows;
use crate::state::{RelationState, RunStats};
use crate::worker::WorkerSet;
use catalog::head::aggregation_catalog_from_program;
use parsing::diagnostic::Result;
use parsing::parser::Program;
use planning::program::ProgramQueryPlan;
use reading::inspect::{MaterializedUpdates, UpdateBatch};
use reading::{KV_MAX, ROW_MAX};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use strata::stratification::Strata;
use tracing::{info, warn};

/// What one evaluation leaves behind for the caller and the explainer.
pub struct RunOutcome {
    pub program: Program,
    pub states: HashMap<String, Arc<RelationState>>,
    pub stats: RunStats,
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

/// Run one program over its input states.
pub fn run(
    engine: &Engine,
    program: Program,
    mut states: HashMap<String, Arc<RelationState>>,
    options: &EvaluationOptions,
) -> Result<RunOutcome> {
    let total_started = Instant::now();
    let profile_run = crate::profile::ProfileRun::start(options.profile_path.as_ref());
    let profile = profile_run.as_ref().map(|run| Arc::clone(&run.profile));
    let config = engine.config();
    let strata = Strata::try_from_parser(program.clone())?;
    debugging::debugger::display_info("Strata", false, format!("{}\n{}", strata.dependency_graph(), strata));
    let (plan, fat_mode) = plan_check(&strata, config)?;
    if let Some(profile) = &profile { profile.phase("compiling_helpers"); }
    let native = native_module(engine, &program)?;
    let idb_map = Arc::new(aggregation_catalog_from_program(&program));
    let budget = budget_of(options);
    let _tag = crate::accounting::MemoryTag::new(budget.memory());
    let workers = WorkerSet::start(config.workers, Arc::clone(&budget), profile.clone())?;
    if let Some(profile) = &profile { profile.phase("executing"); }

    let mut stats = RunStats {
        strata: strata.strata().len(),
        rules: program.rules().len(),
        planning_micros: duration_micros(total_started.elapsed()),
        ..RunStats::default()
    };

    // Every head a rule derives; a sideways slice is internal to its group.
    let mut heads: BTreeMap<String, usize> = BTreeMap::new();
    for rule in program.rules() {
        if !rule.is_sideways() {
            heads.insert(rule.head().name().clone(), rule.head().arity());
        }
    }
    // Every input, and every declared relation a rule reads but none
    // defines: it is empty.
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

    let started = Instant::now();
    workers.run(Arc::new(Assembly {
        groups: plan.program_plan().clone(),
        inputs,
        captures: captures.clone(),
        fat_mode,
        idb_map: Arc::clone(&idb_map),
        native_calls: native.clone(),
        budget: Arc::clone(&budget),
        program_name: program.name().to_string(),
        profile: workers.profile(),
    }))?;
    stats.execution_micros = duration_micros(started.elapsed());

    // Each head settles on its own: merging is independent across heads.
    let started = Instant::now();
    let settled = parallel_map(captures.into_iter().collect(), workers.peers(), &budget,
        |(name, updates): (String, MaterializedUpdates)| {
            let arity = heads[&name];
            let rows = consolidate(&updates, arity, budget.memory());
            budget.note_tuples(rows.len() as u64);
            let state = Arc::new(RelationState::from_sorted(&name, arity, rows));
            (name, state)
        });
    for (name, state) in settled {
        states.insert(name, state);
    }
    stats.settle_micros = duration_micros(started.elapsed());
    drop(workers);
    budget.poll()?;

    stats.peak_memory_bytes = budget.memory().peak();
    stats.materialized_rows = budget.tuples();
    stats.total_micros = duration_micros(total_started.elapsed());
    info!(
        "evaluation: {} strata, {} rules, {} rows settled; {:.3}s preparing, {:.3}s in the dataflow, {:.3}s settling",
        stats.strata,
        stats.rules,
        stats.materialized_rows,
        stats.planning_micros as f64 / 1e6,
        stats.execution_micros as f64 / 1e6,
        stats.settle_micros as f64 / 1e6,
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

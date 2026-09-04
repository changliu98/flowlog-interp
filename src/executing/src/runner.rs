//! Shared preparation path for one-shot and cached execution.

use crate::arg::Args;
use crate::cache::{
    unit_inputs, unit_key, units_of_stratum, CacheEntry, CacheRunStats, HitSource,
    RelationState, StrataCache,
};
use crate::dataflow::{program_execution, stratum_execution};
use crate::native_calls::NativeCallModule;
use catalog::head::aggregation_catalog_from_program;
use parsing::decl::RelDecl;
use parsing::parser::Program;
use parsing::Val;
use planning::program::{plan_rules, ProgramQueryPlan};
use reading::inspect::MaterializedUpdates;
use reading::reader::read_relation_rows;
use reading::{KV_MAX, ROW_MAX};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use strata::stratification::Strata;
use tracing::{info, warn};

/// One evaluation.  With `--cache-dir` it reads along the state store, so
/// successive processes over the same directory share every unchanged unit;
/// without it, it is the classic one-shot dataflow.
pub fn run_once(args: Args) {
    if args.cache_dir().is_some() {
        let cache = Mutex::new(StrataCache::from_args(&args));
        let stats = run_cached(args.clone(), &cache);
        if let Some(csvs) = args.csvs() {
            let directory = format!("{csvs}/csvs");
            let path = format!("{directory}/cache-stats.json");
            let written = fs::create_dir_all(&directory).and_then(|()| {
                let bytes = serde_json::to_vec(&stats)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error))?;
                fs::write(&path, bytes)
            });
            if let Err(error) = written {
                warn!("could not write {path}: {error}");
            }
        }
        return;
    }
    let (strata, program_query_plan, fat_mode) = prepare(&args, args.no_sharing());
    let idb_map = aggregation_catalog_from_program(strata.program());
    program_execution(
        args,
        strata,
        program_query_plan.program_plan().to_owned(),
        fat_mode,
        idb_map,
    );
}

/// The cache behind its lock, poisoned or not: a reload that panicked mid-way
/// wrote nothing partial, so the entries are as good as they were.
pub fn lock(cache: &Mutex<StrataCache>) -> MutexGuard<'_, StrataCache> {
    cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Evaluate by reading along the cache, stratum by stratum.
///
/// Every unit of a stratum reads only states settled by earlier strata, so all
/// of a stratum's keys are computed before any of it runs; the units the cache
/// does not hold are then assembled into one dataflow over the injected input
/// states, captured at their heads, and stored.  Outputs are written from the
/// final states, hit or computed alike.
pub fn run_cached(args: Args, cache: &Mutex<StrataCache>) -> CacheRunStats {
    let total_started = Instant::now();
    // Cross-stratum common-subexpression sharing can leave a later plan referring
    // to an intermediate collection owned by a cached unit. Replanning every rule
    // locally makes relation states a complete boundary.
    let (strata, _program_query_plan, fat_mode) = prepare(&args, true);
    let program = strata.program();
    let idb_map = Arc::new(aggregation_catalog_from_program(program));
    let native_calls = program
        .embedded_rust()
        .map(|embedded| NativeCallModule::compile_and_load(embedded, args.call_cache()))
        .transpose()
        .unwrap_or_else(|error| panic!("failed to prepare .code rust module: {error}"));
    let delimiter = args.delimiter().as_bytes()[0];
    let declarations = program
        .edbs()
        .iter()
        .chain(program.idbs().iter())
        .map(|declaration| (declaration.name(), declaration))
        .collect::<HashMap<&str, &RelDecl>>();

    let mut states: HashMap<String, Arc<RelationState>> = HashMap::new();
    for declaration in program.edbs() {
        let path = match declaration.path() {
            Some(path) => format!("{}/{}", args.facts(), path),
            None => format!("{}/{}.facts", args.facts(), declaration.name()),
        };
        let rows = read_relation_rows(declaration, &path, &delimiter);
        states.insert(
            declaration.name().to_string(),
            Arc::new(RelationState::new(declaration.name(), declaration.arity(), rows)),
        );
    }
    let planning_elapsed = total_started.elapsed();

    let mut stats = CacheRunStats::default();
    let mut execution = Duration::ZERO;
    let mut seen_set = HashSet::new();
    let strata_rules = strata.strata();
    let recursive = strata.is_recursive_strata_bitmap();
    stats.strata = strata_rules.len();

    for (stratum, &is_recursive) in strata_rules.iter().zip(recursive.iter()) {
        let units = units_of_stratum(stratum, is_recursive);
        stats.units += units.len();
        let mut hit_entries = Vec::new();
        let mut missed = Vec::new();
        for unit in units {
            let inputs = unit_inputs(&unit, &states, &declarations);
            let key = unit_key(&unit, &inputs, &declarations, program);
            let found = lock(cache).lookup(&key, &unit.heads);
            match found {
                Some((entry, source)) => {
                    stats.hits += 1;
                    if source == HitSource::Disk {
                        stats.disk_hits += 1;
                    }
                    if stats.misses > 0 {
                        stats.cutoff_hits += 1;
                    }
                    stats.rows_loaded += entry.row_count();
                    hit_entries.push(entry);
                }
                None => {
                    stats.misses += 1;
                    missed.push((unit, key, inputs));
                }
            }
        }

        if !missed.is_empty() {
            let started = Instant::now();
            let mut injected: BTreeMap<String, Arc<RelationState>> = BTreeMap::new();
            let mut unit_groups = Vec::with_capacity(missed.len());
            let mut unit_captures: Vec<BTreeMap<String, MaterializedUpdates>> =
                Vec::with_capacity(missed.len());
            // Sideways slices are named by rule identifier; units assembled
            // into one dataflow must not reuse each other's.
            let mut next_rule_identifier = 0usize;
            for (unit, _, inputs) in &missed {
                for (name, state) in inputs {
                    injected.insert(name.clone(), Arc::clone(state));
                }
                unit_groups.push(plan_rules(
                    &unit.rules,
                    unit.recursive,
                    args.opt_level(),
                    program,
                    next_rule_identifier,
                    &mut seen_set,
                    true,
                ));
                next_rule_identifier += unit.rules.len();
                unit_captures.push(
                    unit.heads
                        .keys()
                        .map(|head| (head.clone(), Arc::new(Mutex::new(Vec::new()))))
                        .collect(),
                );
            }
            stratum_execution(
                &args,
                unit_groups,
                injected,
                unit_captures.clone(),
                fat_mode,
                Arc::clone(&idb_map),
                native_calls.clone(),
            );
            execution += started.elapsed();

            for ((unit, key, _), captures) in missed.into_iter().zip(unit_captures) {
                let mut relations = BTreeMap::new();
                for (name, updates) in captures {
                    let rows = consolidate(&updates);
                    stats.rows_cached += rows.len();
                    let arity = unit.heads[&name];
                    relations.insert(name.clone(), Arc::new(RelationState::new(&name, arity, rows)));
                }
                let entry = Arc::new(CacheEntry::new(relations));
                for (name, state) in &entry.relations {
                    states.insert(name.clone(), Arc::clone(state));
                }
                lock(cache).insert(key, entry);
            }
        }

        for entry in hit_entries {
            for (name, state) in &entry.relations {
                states.insert(name.clone(), Arc::clone(state));
            }
        }
    }

    if let Some(csv_dir) = args.csvs() {
        write_states(program, &states, &csv_dir, delimiter);
    }

    let state = lock(cache).state_stats();
    stats.entries = state.entries;
    stats.resident_rows = state.resident_rows;
    stats.resident_bytes = state.resident_bytes;
    stats.max_bytes = state.max_bytes;
    stats.planning_micros = duration_micros(planning_elapsed);
    stats.execution_micros = duration_micros(execution);
    stats.total_micros = duration_micros(total_started.elapsed());
    info!(
        "state cache: {} strata, {} units, {} hits ({} from disk, {} after a miss), {} misses",
        stats.strata, stats.units, stats.hits, stats.disk_hits, stats.cutoff_hits, stats.misses
    );
    stats
}

/// The rows a capture accumulated, as a set: every row whose total weight is
/// positive once the worker frontier has closed.
fn consolidate(updates: &MaterializedUpdates) -> Vec<Vec<Val>> {
    let mut consolidated = BTreeMap::<Vec<Val>, isize>::new();
    let mut updates = updates.lock().expect("materialized relation lock poisoned");
    for (row, difference) in updates.drain(..) {
        *consolidated.entry(row).or_default() += difference;
    }
    consolidated
        .into_iter()
        .filter_map(|(row, difference)| (difference > 0).then_some(row))
        .collect()
}

/// Write every declared output relation that has a state, the way the one-shot
/// dataflow's sinks write theirs: one row per line, the delimiter between
/// columns, and a size line per non-empty relation.
fn write_states(
    program: &Program,
    states: &HashMap<String, Arc<RelationState>>,
    csv_dir: &str,
    delimiter: u8,
) {
    let directory = format!("{csv_dir}/csvs");
    fs::create_dir_all(&directory)
        .unwrap_or_else(|error| panic!("Can not create output directory {directory}: {error}"));
    let sizes_path = format!("{directory}/size.txt");
    let mut sizes = BufWriter::new(
        File::create(&sizes_path)
            .unwrap_or_else(|error| panic!("Can not create output file {sizes_path}: {error}")),
    );
    for idb in program.idbs() {
        let Some(state) = states.get(idb.name()) else {
            continue;
        };
        let path = format!("{directory}/{}.csv", idb.name());
        let mut out = BufWriter::new(
            File::create(&path)
                .unwrap_or_else(|error| panic!("Can not create output file {path}: {error}")),
        );
        for row in state.rows.iter() {
            for (column, value) in row.iter().enumerate() {
                if column > 0 {
                    out.write_all(std::slice::from_ref(&delimiter))
                        .unwrap_or_else(|error| panic!("Can not write to {path}: {error}"));
                }
                write!(out, "{value}").unwrap_or_else(|error| panic!("Can not write to {path}: {error}"));
            }
            out.write_all(b"\n")
                .unwrap_or_else(|error| panic!("Can not write to {path}: {error}"));
        }
        out.flush()
            .unwrap_or_else(|error| panic!("Can not write to {path}: {error}"));
        if !state.rows.is_empty() {
            writeln!(sizes, "{}: ((), (), {})", idb.name(), state.rows.len())
                .unwrap_or_else(|error| panic!("Can not write size to {sizes_path}: {error}"));
        }
        info!("Size of [{}]: {}", idb.name(), state.rows.len());
    }
    sizes
        .flush()
        .unwrap_or_else(|error| panic!("Can not write size to {sizes_path}: {error}"));
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn prepare(args: &Args, disable_sharing: bool) -> (Strata, ProgramQueryPlan, bool) {
    let program = parsing::parser::Program::from_str(args.program());
    debugging::debugger::display_info("Parsed Program", false, format!("{}", program));

    let strata = Strata::from_parser(program);
    debugging::debugger::display_info(
        "Strata",
        false,
        format!("{}\n{}", strata.dependency_graph(), strata),
    );

    let program_query_plan =
        ProgramQueryPlan::from_strata(&strata, disable_sharing, args.opt_level());
    debugging::debugger::display_info(
        "Program Query Plans",
        true,
        format!("{}", program_query_plan),
    );
    debugging::debugger::display_info(
        "Arity Checks",
        false,
        format!(
            "Maximum arity required: {}\nMaximal incomparable (key, value) arity pairs: {:?}\nMax arities per transformation:\n{}",
            program_query_plan.max_arity(),
            program_query_plan.maximal_arity_pairs(),
            program_query_plan
                .arity_analysis()
                .iter()
                .map(|(name, inputs, output)| {
                    format!("  {} @ inputs: {:?} -> output: {:?}", name, inputs, output)
                })
                .collect::<Vec<_>>()
                .join("\n")
        ),
    );

    let fat_mode = program_query_plan.should_use_fat_mode(args.fat_mode(), KV_MAX, ROW_MAX);
    if fat_mode && !args.fat_mode() {
        warn!("WARNING: Fat mode automatically enabled due to high arity");
        warn!(
            "         Maximal incomparable arity pairs found: {:?}",
            program_query_plan.maximal_arity_pairs()
        );
    }

    (strata, program_query_plan, fat_mode)
}

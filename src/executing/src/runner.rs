//! Shared preparation path for one-shot and resident-cache execution.

use crate::arg::Args;
use crate::cache::{CacheContext, CacheRunStats, StrataCache};
use crate::dataflow::{program_execution, program_execution_cached};
use catalog::head::aggregation_catalog_from_program;
use planning::program::ProgramQueryPlan;
use reading::{KV_MAX, ROW_MAX};
use std::path::Path;
use std::sync::Arc;
use strata::stratification::Strata;
use tracing::warn;

pub fn run_once(args: Args) {
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

pub fn run_cached(args: Args, cache: &mut StrataCache) -> CacheRunStats {
    let total_started = std::time::Instant::now();
    // Cross-stratum common-subexpression sharing can leave a later plan referring
    // to an intermediate collection owned by a cached stratum. Replanning every
    // rule locally makes materialized relation heads a complete cache boundary.
    let (strata, program_query_plan, fat_mode) = prepare(&args, true);
    let idb_map = aggregation_catalog_from_program(strata.program());
    let facts = args.facts();
    let prepared = Arc::new(
        cache
            .prepare(
                strata.program(),
                program_query_plan.program_plan(),
                CacheContext {
                    facts: Path::new(&facts),
                    delimiter: args.delimiter().as_bytes()[0],
                    fat_mode,
                    opt_level: args.opt_level(),
                },
            )
            .unwrap_or_else(|error| panic!("cannot prepare resident cache: {error}")),
    );
    let planning_elapsed = total_started.elapsed();

    let execution_started = std::time::Instant::now();
    program_execution_cached(
        args,
        strata,
        program_query_plan.program_plan().to_owned(),
        fat_mode,
        idb_map,
        Arc::clone(&prepared),
    );
    let execution_elapsed = execution_started.elapsed();
    prepared
        .verify_inputs()
        .unwrap_or_else(|error| panic!("cannot commit resident cache: {error}"));
    let mut stats = cache.commit(&prepared);
    stats.planning_micros = duration_micros(planning_elapsed);
    stats.execution_micros = duration_micros(execution_elapsed);
    stats.total_micros = duration_micros(total_started.elapsed());
    stats
}

fn duration_micros(duration: std::time::Duration) -> u64 {
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

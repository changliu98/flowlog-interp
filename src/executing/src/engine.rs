//! The engine: what a host holds to evaluate programs.
//!
//! An `Engine` owns the state that outlives one evaluation - the memory tier
//! of the state cache, the optional disk tier, the symbol table, the
//! admission limit - and offers three operations: `check` a program without
//! evaluating it, `evaluate` a program over inputs, and read or make
//! symbols. The command line, the service and the C API are all clients of
//! this type; none of them holds anything it does not.
//!
//! A request names its program (text, or already parsed), its inputs (rows in
//! memory, or a directory of files), and its options: the limits it runs
//! under, whether to read along the cache, which schedule to run, and which
//! rows to explain. A result holds every relation's final state, the column
//! types to read them by, the run's counters, and the witnesses asked for.

use crate::accounting::{Budget, Limits};
use crate::cache::{CacheRunStats, CacheStateStats, DiskStore, RelationState, StrataCache};
use crate::explain::{Explainer, Witness};
use crate::files;
use crate::runner;
use crate::symbols::SymbolTable;
use parsing::decl::DataType;
use parsing::diagnostic::{Diagnostic, Result};
use parsing::parser::Program;
use parsing::Val;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use strata::stratification::Strata;

/// How an engine is set up. Every field has a default; a host sets what it
/// cares about.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// Worker threads per evaluation.
    pub workers: usize,
    /// Plan every collection onto heap rows, whatever its arity.
    pub fat_mode: bool,
    /// Share common subexpressions across the rules of one dataflow.
    pub sharing: bool,
    /// Optimization level (0-3): sideways information passing and structural
    /// planning; `None` follows each rule's own hints.
    pub opt_level: Option<u8>,
    /// Bytes the memory tier of the state cache may hold.
    pub cache_memory_bytes: usize,
    /// A directory for the disk tier of the state cache; none by default.
    pub cache_dir: Option<PathBuf>,
    /// Bytes the disk tier may hold.
    pub cache_disk_bytes: u64,
    /// A directory for compiled embedded blocks; the platform cache by default.
    pub call_cache: Option<PathBuf>,
    /// Evaluations allowed to run at once; 0 for no limit.
    pub max_concurrent: usize,
    /// The column delimiter of relation files.
    pub delimiter: u8,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            workers: 1,
            fat_mode: false,
            sharing: true,
            opt_level: None,
            cache_memory_bytes: 4 * 1024 * 1024 * 1024,
            cache_dir: None,
            cache_disk_bytes: 32 * 1024 * 1024 * 1024,
            call_cache: None,
            max_concurrent: 0,
            delimiter: b',',
        }
    }
}

impl EngineConfig {
    pub fn from_json(text: &str) -> Result<Self> {
        serde_json::from_str(text)
            .map_err(|error| Diagnostic::validation(format!("invalid engine configuration: {error}")))
    }
}

/// The program of a request.
#[derive(Debug, Clone)]
pub enum ProgramSource {
    Text { name: String, source: String },
    Parsed(Program),
}

/// The inputs of a request.
#[derive(Debug, Clone)]
pub enum Inputs {
    /// Rows in memory, by relation name, as cells.
    Rows(BTreeMap<String, Vec<Vec<Val>>>),
    /// A directory of relation files, read by each input declaration's path.
    Directory(PathBuf),
    /// States already built.
    States(HashMap<String, Arc<RelationState>>),
}

/// How an evaluation is scheduled onto dataflows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Schedule {
    /// One dataflow per stratum, each keyed against the state cache.
    PerStratum,
    /// One dataflow for the whole program, sharing intermediates across
    /// strata; never cached.
    WholeProgram,
}

/// What an evaluation may do and how far it reports.
#[derive(Debug, Clone, Default)]
pub struct EvaluationOptions {
    pub limits: Limits,
    /// Read along and write into the state cache. `None` reads along it.
    pub cache: Option<bool>,
    /// `None` is per-stratum when caching, whole-program otherwise.
    pub schedule: Option<Schedule>,
    /// Rows to explain: relation and row, each answered with a witness.
    pub explain: Vec<(String, Vec<Val>)>,
    /// Explain every derived row of every output relation.
    pub explain_all: bool,
}

impl EvaluationOptions {
    pub fn cached(&self) -> bool {
        self.cache.unwrap_or(true)
    }

    pub fn schedule(&self) -> Schedule {
        self.schedule.unwrap_or(if self.cached() {
            Schedule::PerStratum
        } else {
            Schedule::WholeProgram
        })
    }
}

#[derive(Debug, Clone)]
pub struct EvaluationRequest {
    pub program: ProgramSource,
    pub inputs: Inputs,
    pub options: EvaluationOptions,
}

/// What an evaluation produced.
#[derive(Debug)]
pub struct EvaluationResult {
    pub program_name: String,
    /// The final state of every relation the program mentions.
    pub outputs: BTreeMap<String, Arc<RelationState>>,
    /// The column types to read the states by.
    pub types: HashMap<String, Vec<DataType>>,
    /// The names of the declared output relations, in declaration order.
    pub declared_outputs: Vec<String>,
    pub stats: CacheRunStats,
    pub witnesses: Vec<Witness>,
}

impl EvaluationResult {
    /// The declared output relations and their states.
    pub fn declared(&self) -> impl Iterator<Item = (&str, &Arc<RelationState>)> {
        self.declared_outputs
            .iter()
            .filter_map(|name| self.outputs.get(name).map(|state| (name.as_str(), state)))
    }

    /// One relation's rows as JSON values: numbers as numbers, symbols as
    /// their texts.
    pub fn rows_json(&self, engine: &Engine, relation: &str) -> Option<serde_json::Value> {
        let state = self.outputs.get(relation)?;
        let types = self.types.get(relation)?;
        Some(serde_json::Value::Array(
            state
                .rows
                .iter()
                .map(|row| {
                    serde_json::Value::Array(
                        row.iter()
                            .zip(types.iter())
                            .map(|(cell, column)| cell_json(engine, *cell, *column))
                            .collect(),
                    )
                })
                .collect(),
        ))
    }
}

/// A cell as JSON: a number, or a symbol's text (its id when the engine
/// does not know the text).
pub fn cell_json(engine: &Engine, cell: Val, column: DataType) -> serde_json::Value {
    match column {
        DataType::Integer => serde_json::Value::from(cell),
        DataType::Symbol => match engine.symbol_text(cell) {
            Some(text) => serde_json::Value::from(text),
            None => serde_json::Value::from(cell),
        },
    }
}

/// One relation of a checked program.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationReport {
    pub name: String,
    pub columns: Vec<DataType>,
    pub input: bool,
    pub output: bool,
    pub derived: bool,
}

/// What `check` says about a program that passed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgramReport {
    pub name: String,
    pub relations: Vec<RelationReport>,
    pub rules: usize,
    pub strata: usize,
    pub recursive_strata: usize,
    pub functions: Vec<String>,
}

/// The admission limit: how many evaluations may run at once.
#[derive(Debug, Default)]
struct Admission {
    running: Mutex<usize>,
    changed: Condvar,
}

struct AdmissionGuard<'a> {
    admission: &'a Admission,
}

impl Admission {
    fn acquire(&self, limit: usize) -> AdmissionGuard<'_> {
        let mut running = self.running.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        while limit > 0 && *running >= limit {
            running = self
                .changed
                .wait(running)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *running += 1;
        AdmissionGuard { admission: self }
    }

    fn running(&self) -> usize {
        *self.running.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for AdmissionGuard<'_> {
    fn drop(&mut self) {
        let mut running = self
            .admission
            .running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *running -= 1;
        self.admission.changed.notify_one();
    }
}

/// The engine.
pub struct Engine {
    config: EngineConfig,
    cache: Mutex<StrataCache>,
    symbols: Arc<SymbolTable>,
    admission: Admission,
    evaluations: AtomicU64,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Arc<Self> {
        let cache = StrataCache::new(config.cache_memory_bytes);
        let cache = match &config.cache_dir {
            Some(directory) => {
                cache.with_disk(DiskStore::new(directory.clone(), config.cache_disk_bytes))
            }
            None => cache,
        };
        Arc::new(Self {
            config,
            cache: Mutex::new(cache),
            symbols: Arc::new(SymbolTable::new()),
            admission: Admission::default(),
            evaluations: AtomicU64::new(0),
        })
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    pub fn symbols(&self) -> &Arc<SymbolTable> {
        &self.symbols
    }

    /// The id of a text, interning it.
    pub fn intern(&self, text: &str) -> Result<Val> {
        self.symbols.intern(text)
    }

    /// The text of a symbol id this engine has seen.
    pub fn symbol_text(&self, id: Val) -> Option<String> {
        self.symbols.resolve(id)
    }

    pub(crate) fn cache(&self) -> &Mutex<StrataCache> {
        &self.cache
    }

    pub fn cache_stats(&self) -> CacheStateStats {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .state_stats()
    }

    /// Evaluations running now.
    pub fn running(&self) -> usize {
        self.admission.running()
    }

    /// Evaluations completed since the engine started.
    pub fn completed(&self) -> u64 {
        self.evaluations.load(Ordering::Relaxed)
    }

    /// Parse, validate, stratify and plan a program without evaluating it.
    pub fn check(&self, source: &str, name: &str) -> Result<ProgramReport> {
        catch_engine_panic(|| self.check_inner(source, name))
    }

    fn check_inner(&self, source: &str, name: &str) -> Result<ProgramReport> {
        let program = self.prepare(ProgramSource::Text {
            name: name.to_string(),
            source: source.to_string(),
        })?;
        let strata = Strata::try_from_parser(program.clone())?;
        runner::plan_check(&strata, &self.config)?;
        let inputs = program
            .edbs()
            .iter()
            .map(|declaration| declaration.name().to_string())
            .collect::<Vec<_>>();
        let outputs = program
            .idbs()
            .iter()
            .map(|declaration| declaration.name().to_string())
            .collect::<Vec<_>>();
        let derived = program
            .rules()
            .iter()
            .map(|rule| rule.head().name().to_string())
            .collect::<std::collections::HashSet<_>>();
        let mut relations = program
            .relation_types()
            .iter()
            .map(|(name, columns)| RelationReport {
                name: name.clone(),
                columns: columns.clone(),
                input: inputs.contains(name),
                output: outputs.contains(name),
                derived: derived.contains(name),
            })
            .collect::<Vec<_>>();
        relations.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(ProgramReport {
            name: program.name().to_string(),
            relations,
            rules: program.rules().len(),
            strata: strata.strata().len(),
            recursive_strata: strata
                .is_recursive_strata_bitmap()
                .iter()
                .filter(|recursive| **recursive)
                .count(),
            functions: program
                .embedded_rust()
                .map(|embedded| {
                    embedded
                        .functions()
                        .iter()
                        .map(|function| function.signature())
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// A program ready to plan: parsed, validated, its text constants lowered
    /// to this engine's symbols.
    pub fn prepare(&self, source: ProgramSource) -> Result<Program> {
        let mut program = match source {
            ProgramSource::Text { name, source } => Program::parse(&source, &name)?,
            ProgramSource::Parsed(program) => program,
        };
        let symbols = Arc::clone(&self.symbols);
        program.lower_symbols(&mut |text| symbols.intern(text))?;
        Ok(program)
    }

    /// Evaluate one program over its inputs. Never panics: a defect in the
    /// engine's own code is returned as an internal diagnostic.
    pub fn evaluate(&self, request: EvaluationRequest) -> Result<EvaluationResult> {
        catch_engine_panic(move || self.evaluate_inner(request))
    }

    fn evaluate_inner(&self, request: EvaluationRequest) -> Result<EvaluationResult> {
        let EvaluationRequest {
            program,
            inputs,
            options,
        } = request;
        let program = self.prepare(program)?;
        let states = self.states_of(&program, inputs)?;

        let _admitted = self.admission.acquire(self.config.max_concurrent);
        let outcome = runner::run(self, program, states, &options)?;

        let witnesses = if options.explain.is_empty() && !options.explain_all {
            Vec::new()
        } else {
            let explainer = Explainer::new(
                &outcome.program,
                &outcome.states,
                outcome.native.as_ref(),
                &outcome.budget,
            );
            let mut requests = options.explain.clone();
            if options.explain_all {
                for declaration in outcome.program.idbs() {
                    if let Some(state) = outcome.states.get(declaration.name()) {
                        for row in state.rows.iter() {
                            requests.push((declaration.name().to_string(), row.clone()));
                        }
                    }
                }
            }
            let mut witnesses = Vec::with_capacity(requests.len());
            for (relation, row) in requests {
                witnesses.push(explainer.explain(&relation, &row)?);
            }
            witnesses
        };

        self.evaluations.fetch_add(1, Ordering::Relaxed);
        Ok(EvaluationResult {
            program_name: outcome.program.name().to_string(),
            declared_outputs: outcome
                .program
                .idbs()
                .iter()
                .map(|declaration| declaration.name().to_string())
                .collect(),
            types: outcome.program.relation_types().clone(),
            outputs: outcome.states.into_iter().collect(),
            stats: outcome.stats,
            witnesses,
        })
    }

    fn states_of(
        &self,
        program: &Program,
        inputs: Inputs,
    ) -> Result<HashMap<String, Arc<RelationState>>> {
        let mut states = match inputs {
            Inputs::States(states) => states,
            Inputs::Directory(directory) => files::read_facts_directory(
                program,
                &directory,
                self.config.delimiter,
                &self.symbols,
            )?,
            Inputs::Rows(rows) => {
                let mut states = HashMap::new();
                for (name, rows) in rows {
                    let Some(columns) = program.column_types(&name) else {
                        return Err(Diagnostic::input(format!(
                            "input relation {name} is not declared by the program"
                        ))
                        .with_relation(name));
                    };
                    let arity = columns.len();
                    for row in &rows {
                        if row.len() != arity {
                            return Err(Diagnostic::input(format!(
                                "input relation {name} was given a row of {} cells, but its \
                                 arity is {arity}",
                                row.len()
                            ))
                            .with_relation(name));
                        }
                    }
                    states.insert(name.clone(), Arc::new(RelationState::new(&name, arity, rows)));
                }
                states
            }
        };
        // An input the request did not supply is empty.
        for declaration in program.edbs() {
            states.entry(declaration.name().to_string()).or_insert_with(|| {
                Arc::new(RelationState::empty(declaration.name(), declaration.arity()))
            });
        }
        Ok(states)
    }
}

/// The counters of the last run and the cache's state, for a status reply.
#[derive(Debug, Clone, Serialize)]
pub struct EngineStatus {
    pub running: usize,
    pub completed: u64,
    pub cache: CacheStateStats,
    pub symbols: usize,
}

impl Engine {
    pub fn status(&self) -> EngineStatus {
        EngineStatus {
            running: self.running(),
            completed: self.completed(),
            cache: self.cache_stats(),
            symbols: self.symbols.len(),
        }
    }
}

/// A budget from a request's limits.
pub(crate) fn budget_of(options: &EvaluationOptions) -> Arc<Budget> {
    Budget::new(&options.limits)
}

/// Run an engine operation, turning a panic in engine code into the
/// diagnostic it should have been. The engine's shared state is behind
/// locks that recover from poisoning, and an evaluation that panicked
/// published nothing partial, so continuing after one is sound.
fn catch_engine_panic<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)) {
        Ok(outcome) => outcome,
        Err(panic) => {
            let message = if let Some(text) = panic.downcast_ref::<String>() {
                text.clone()
            } else if let Some(text) = panic.downcast_ref::<&str>() {
                (*text).to_string()
            } else {
                "the engine panicked without a text message".to_string()
            };
            Err(Diagnostic::internal(format!(
                "the engine failed inside its own code, which is a defect of the engine and \
                 not a verdict on the program: {message}"
            ))
            .with_detail(message))
        }
    }
}

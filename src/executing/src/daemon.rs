//! The service: one engine on a Unix-domain socket, for hosts that are not
//! in this process.
//!
//! Protocol version 2. One request per line, one JSON object; one response
//! per line, one JSON object. Every response carries `version`, `ok`, the
//! `command` it answers, the request's `id` when one was given, the engine's
//! `cache` occupancy, and on failure a structured `diagnostic` beside its
//! rendering in `error`. Evaluations run concurrently, each on its own
//! thread, under the engine's admission limit; `cancel` stops one by id.
//!
//! Commands:
//!
//! - `evaluate`: `program` (`{"path"}` or `{"text", "name"}`), `inputs`
//!   (`{"facts": dir}` or `{"rows": {relation: [[cell, ...], ...]}}`, where a
//!   symbol cell is its text), `output` (`{"csvs": dir}` and/or
//!   `{"inline": true}`), `options` (`budget_seconds`, `memory_limit_bytes`,
//!   `tuple_limit`, `cache`, `schedule`, `explain`, `explain_all`).
//! - `check`: `program` as above; answers with the program report.
//! - `reload`: the version 1 form, `program`, `facts` and `csvs` paths
//!   defaulting to the ones the service was started with.
//! - `cancel`: `id` of an evaluation in flight.
//! - `stats`, `shutdown`.

use crate::accounting::{CancelToken, Limits};
use crate::cache::{CacheRunStats, CacheStateStats};
use crate::engine::{
    Engine, EvaluationOptions, EvaluationRequest, Inputs, ProgramReport, ProgramSource, Schedule,
};
use crate::explain::Witness;
use crate::files;
use parsing::decl::DataType;
use parsing::diagnostic::{Diagnostic, Result};
use parsing::parser::Program;
use parsing::Val;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::{info, warn};

pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProgramSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct InputSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<BTreeMap<String, Vec<Vec<serde_json::Value>>>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct OutputSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub csvs: Option<String>,
    #[serde(default)]
    pub inline: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ExplainSpec {
    pub relation: String,
    pub row: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct OptionsSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_limit_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tuple_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<Schedule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub explain: Vec<ExplainSpec>,
    #[serde(default)]
    pub explain_all: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum DaemonRequest {
    /// The version 1 form: evaluate the service's program over its facts, or
    /// the named ones.
    Reload {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        facts: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        csvs: Option<String>,
    },
    Evaluate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        program: ProgramSpec,
        #[serde(default)]
        inputs: InputSpec,
        #[serde(default)]
        output: OutputSpec,
        #[serde(default)]
        options: OptionsSpec,
    },
    Check {
        program: ProgramSpec,
    },
    Cancel {
        id: String,
    },
    Stats,
    Shutdown,
}

impl DaemonRequest {
    pub fn reload() -> Self {
        Self::Reload {
            program: None,
            facts: None,
            csvs: None,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DaemonResponse {
    pub version: u32,
    pub ok: bool,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<CacheRunStats>,
    pub cache: CacheStateStats,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<ProgramReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub types: Option<BTreeMap<String, Vec<DataType>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub witnesses: Option<Vec<Witness>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<Diagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running: Option<usize>,
}

impl DaemonResponse {
    fn new(engine: &Engine, command: &str, id: Option<String>) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            ok: true,
            command: command.to_string(),
            id,
            run: None,
            cache: engine.cache_stats(),
            report: None,
            outputs: None,
            types: None,
            witnesses: None,
            diagnostic: None,
            error: None,
            running: None,
        }
    }

    fn failed(mut self, diagnostic: Diagnostic) -> Self {
        self.ok = false;
        self.error = Some(diagnostic.to_string());
        self.diagnostic = Some(diagnostic);
        self
    }
}

/// What the service was started with: the defaults of a `reload`.
#[derive(Debug, Clone)]
pub struct ServiceDefaults {
    pub program: Option<PathBuf>,
    pub facts: Option<PathBuf>,
    pub csvs: Option<PathBuf>,
}

struct InFlight {
    cancel: CancelToken,
}

pub struct Service {
    engine: Arc<Engine>,
    defaults: ServiceDefaults,
    in_flight: Mutex<HashMap<String, InFlight>>,
    last_run: Mutex<Option<CacheRunStats>>,
}

/// Serve `engine` on `socket_path` until `shutdown`.
pub fn serve(engine: Arc<Engine>, defaults: ServiceDefaults, socket_path: PathBuf) -> io::Result<()> {
    if socket_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "refusing to replace existing daemon socket {}",
                socket_path.display()
            ),
        ));
    }

    let listener = UnixListener::bind(&socket_path)?;
    let _socket_guard = SocketGuard(socket_path.clone());
    let service = Arc::new(Service {
        engine,
        defaults,
        in_flight: Mutex::new(HashMap::new()),
        last_run: Mutex::new(None),
    });
    let mut handles: Vec<JoinHandle<()>> = Vec::new();
    info!(
        "FlowLog service listening on {} (protocol {PROTOCOL_VERSION}, memory tier {} bytes{})",
        socket_path.display(),
        service.engine.config().cache_memory_bytes,
        service
            .engine
            .config()
            .cache_dir
            .as_ref()
            .map(|directory| format!(", disk tier {}", directory.display()))
            .unwrap_or_default()
    );

    for connection in listener.incoming() {
        let mut stream = match connection {
            Ok(stream) => stream,
            Err(error) => {
                warn!("service accept failed: {error}");
                continue;
            }
        };
        handles.retain(|handle| !handle.is_finished());

        let request = read_request(&stream);
        match request {
            Ok(DaemonRequest::Shutdown) => {
                let mut response = DaemonResponse::new(&service.engine, "shutdown", None);
                response.run = service.last_run.lock().unwrap_or_else(|p| p.into_inner()).clone();
                if let Err(error) = write_response(&mut stream, &response) {
                    warn!("service could not write a response: {error}");
                }
                break;
            }
            Ok(DaemonRequest::Stats) => {
                let mut response = DaemonResponse::new(&service.engine, "stats", None);
                response.run = service.last_run.lock().unwrap_or_else(|p| p.into_inner()).clone();
                response.running = Some(service.engine.running());
                if let Err(error) = write_response(&mut stream, &response) {
                    warn!("service could not write a response: {error}");
                }
            }
            Ok(DaemonRequest::Cancel { id }) => {
                let cancelled = service
                    .in_flight
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(&id)
                    .map(|flight| flight.cancel.cancel())
                    .is_some();
                let response = DaemonResponse::new(&service.engine, "cancel", Some(id.clone()));
                let response = if cancelled {
                    response
                } else {
                    response.failed(Diagnostic::validation(format!(
                        "no evaluation with id {id:?} is in flight"
                    )))
                };
                if let Err(error) = write_response(&mut stream, &response) {
                    warn!("service could not write a response: {error}");
                }
            }
            Ok(DaemonRequest::Check { program }) => {
                let response = match resolve_program(&program, &service.defaults) {
                    Ok((text, name)) => match service.engine.check(&text, &name) {
                        Ok(report) => {
                            let mut response = DaemonResponse::new(&service.engine, "check", None);
                            response.report = Some(report);
                            response
                        }
                        Err(diagnostic) => {
                            DaemonResponse::new(&service.engine, "check", None).failed(diagnostic)
                        }
                    },
                    Err(diagnostic) => {
                        DaemonResponse::new(&service.engine, "check", None).failed(diagnostic)
                    }
                };
                if let Err(error) = write_response(&mut stream, &response) {
                    warn!("service could not write a response: {error}");
                }
            }
            Ok(request @ (DaemonRequest::Reload { .. } | DaemonRequest::Evaluate { .. })) => {
                let service = Arc::clone(&service);
                handles.push(std::thread::spawn(move || {
                    let response = service.evaluate(request);
                    if let Err(error) = write_response(&mut stream, &response) {
                        warn!("service could not write a response: {error}");
                    }
                }));
            }
            Err(error) => {
                let response = DaemonResponse::new(&service.engine, "invalid", None)
                    .failed(Diagnostic::validation(error.to_string()));
                if let Err(error) = write_response(&mut stream, &response) {
                    warn!("service could not write a response: {error}");
                }
            }
        }
    }

    for handle in handles {
        let _ = handle.join();
    }
    Ok(())
}

impl Service {
    fn evaluate(&self, request: DaemonRequest) -> DaemonResponse {
        let (command, id, program, inputs, output, options) = match request {
            DaemonRequest::Reload {
                program,
                facts,
                csvs,
            } => (
                "reload",
                None,
                ProgramSpec {
                    path: program,
                    text: None,
                    name: None,
                },
                InputSpec {
                    facts: facts.or_else(|| {
                        self.defaults
                            .facts
                            .as_ref()
                            .map(|path| path.to_string_lossy().to_string())
                    }),
                    rows: None,
                },
                OutputSpec {
                    csvs: csvs.or_else(|| {
                        self.defaults
                            .csvs
                            .as_ref()
                            .map(|path| path.to_string_lossy().to_string())
                    }),
                    inline: false,
                },
                OptionsSpec::default(),
            ),
            DaemonRequest::Evaluate {
                id,
                program,
                inputs,
                output,
                options,
            } => ("evaluate", id, program, inputs, output, options),
            _ => unreachable!("only evaluations reach Service::evaluate"),
        };
        let mut response = DaemonResponse::new(&self.engine, command, id.clone());
        let cancel = CancelToken::new();
        if let Some(id) = &id {
            self.in_flight
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(id.clone(), InFlight { cancel: cancel.clone() });
        }
        let outcome = self.run(program, inputs, &output, options, cancel);
        if let Some(id) = &id {
            self.in_flight
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(id);
        }
        match outcome {
            Ok((run, outputs, types, witnesses)) => {
                *self.last_run.lock().unwrap_or_else(|p| p.into_inner()) = Some(run.clone());
                response.run = Some(run);
                response.cache = self.engine.cache_stats();
                if output.inline {
                    response.outputs = Some(outputs);
                    response.types = Some(types);
                }
                if !witnesses.is_empty() {
                    response.witnesses = Some(witnesses);
                }
                response
            }
            Err(diagnostic) => {
                response.cache = self.engine.cache_stats();
                response.failed(diagnostic)
            }
        }
    }

    fn run(
        &self,
        program: ProgramSpec,
        inputs: InputSpec,
        output: &OutputSpec,
        options: OptionsSpec,
        cancel: CancelToken,
    ) -> Result<(
        CacheRunStats,
        BTreeMap<String, serde_json::Value>,
        BTreeMap<String, Vec<DataType>>,
        Vec<Witness>,
    )> {
        let (text, name) = resolve_program(&program, &self.defaults)?;
        let parsed = Program::parse(&text, &name)?;
        let program = parsed.clone();
        let engine_inputs = match (inputs.facts, inputs.rows) {
            (Some(facts), _) => Inputs::Directory(PathBuf::from(facts)),
            (None, Some(rows)) => Inputs::Rows(rows_of(&self.engine, &parsed, rows)?),
            (None, None) => Inputs::Rows(BTreeMap::new()),
        };
        let explain = options
            .explain
            .iter()
            .map(|spec| {
                let types = parsed.column_types(&spec.relation).ok_or_else(|| {
                    Diagnostic::validation(format!(
                        "cannot explain {}: the program does not mention it",
                        spec.relation
                    ))
                })?;
                Ok((spec.relation.clone(), cells_of(&self.engine, &spec.relation, types, &spec.row)?))
            })
            .collect::<Result<Vec<_>>>()?;
        let request = EvaluationRequest {
            program: ProgramSource::Parsed(parsed),
            inputs: engine_inputs,
            options: EvaluationOptions {
                limits: Limits {
                    time: options.budget_seconds.map(Duration::from_secs_f64),
                    cancel: Some(cancel),
                    memory_bytes: options.memory_limit_bytes,
                    tuples: options.tuple_limit,
                },
                cache: options.cache,
                schedule: options.schedule,
                explain,
                explain_all: options.explain_all,
            },
        };
        let result = self.engine.evaluate(request)?;
        if let Some(csvs) = &output.csvs {
            let directory = Path::new(csvs);
            let states = result
                .outputs
                .iter()
                .map(|(name, state)| (name.clone(), Arc::clone(state)))
                .collect();
            files::write_outputs(
                &program,
                &states,
                directory,
                self.engine.config().delimiter,
                self.engine.symbols(),
            )?;
            files::write_stats(directory, &result.stats)?;
            if !result.witnesses.is_empty() {
                files::write_witnesses(directory, &result.witnesses)?;
            }
        }
        let mut outputs = BTreeMap::new();
        let mut types = BTreeMap::new();
        if output.inline {
            for (name, _) in result.declared() {
                if let Some(rows) = result.rows_json(&self.engine, name) {
                    outputs.insert(name.to_string(), rows);
                    types.insert(name.to_string(), result.types[name].clone());
                }
            }
        }
        Ok((result.stats, outputs, types, result.witnesses))
    }
}

fn resolve_program(spec: &ProgramSpec, defaults: &ServiceDefaults) -> Result<(String, String)> {
    if let Some(text) = &spec.text {
        return Ok((text.clone(), spec.name.clone().unwrap_or_else(|| "(request)".to_string())));
    }
    let path = match &spec.path {
        Some(path) => PathBuf::from(path),
        None => defaults.program.clone().ok_or_else(|| {
            Diagnostic::validation("the request names no program and the service has no default")
        })?,
    };
    let text = fs::read_to_string(&path).map_err(|error| {
        Diagnostic::parse(format!("can't read program from \"{}\": {error}", path.display()))
    })?;
    Ok((text, path.to_string_lossy().to_string()))
}

/// JSON rows to cells, by the relation's column types: numbers as they are,
/// symbol texts interned.
fn rows_of(
    engine: &Engine,
    program: &Program,
    rows: BTreeMap<String, Vec<Vec<serde_json::Value>>>,
) -> Result<BTreeMap<String, Vec<Vec<Val>>>> {
    let mut cells = BTreeMap::new();
    for (relation, rows) in rows {
        let Some(types) = program.column_types(&relation) else {
            return Err(Diagnostic::input(format!(
                "input relation {relation} is not declared by the program"
            ))
            .with_relation(relation));
        };
        let mut converted = Vec::with_capacity(rows.len());
        for row in &rows {
            converted.push(cells_of(engine, &relation, types, row)?);
        }
        cells.insert(relation, converted);
    }
    Ok(cells)
}

fn cells_of(engine: &Engine, relation: &str, types: &[DataType], row: &[serde_json::Value]) -> Result<Vec<Val>> {
    if row.len() != types.len() {
        return Err(Diagnostic::input(format!(
            "a row of {relation} has {} cells, but its arity is {}",
            row.len(),
            types.len()
        ))
        .with_relation(relation));
    }
    row.iter()
        .zip(types.iter())
        .map(|(value, column)| match (column, value) {
            (DataType::Integer, serde_json::Value::Number(number)) => number.as_i64().ok_or_else(|| {
                Diagnostic::input(format!("cell {number} of {relation} is not a 64-bit integer"))
            }),
            (DataType::Symbol, serde_json::Value::String(text)) => engine.intern(text),
            (DataType::Symbol, serde_json::Value::Number(number)) => number.as_i64().ok_or_else(|| {
                Diagnostic::input(format!("cell {number} of {relation} is not a symbol id"))
            }),
            (column, value) => Err(Diagnostic::input(format!(
                "cell {value} of {relation} is not a {column}"
            ))
            .with_relation(relation)),
        })
        .collect()
}

fn write_response(stream: &mut UnixStream, response: &DaemonResponse) -> io::Result<()> {
    serde_json::to_writer(&mut *stream, response)?;
    stream.write_all(b"\n")?;
    stream.flush()
}

/// Send one request and read one response.
pub fn request(socket_path: &Path, request: DaemonRequest) -> io::Result<String> {
    let mut stream = UnixStream::connect(socket_path)?;
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    Ok(response)
}

fn read_request(stream: &UnixStream) -> io::Result<DaemonRequest> {
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid daemon request: {error}"),
        )
    })
}

struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

//! The C interface: the engine for a host that is not written in Rust.
//!
//! A host creates an engine from a JSON configuration, hands it programs as
//! text and inputs as flat cell arrays, and reads results back as flat cell
//! arrays with the column types to interpret them. Symbols cross the boundary
//! as ids; `flowlog_intern` and `flowlog_symbol` translate. Every failure is
//! a JSON diagnostic string the host frees with `flowlog_string_free`.
//!
//! ```c
//! typedef struct flowlog_engine flowlog_engine;
//! typedef struct flowlog_result flowlog_result;
//! typedef struct { const char* relation; size_t arity; const int64_t* cells; size_t rows; } flowlog_input;
//!
//! flowlog_engine* flowlog_engine_new(const char* config_json, char** error_json);
//! void            flowlog_engine_free(flowlog_engine*);
//! char*           flowlog_check(flowlog_engine*, const char* program, const char* name);
//! int64_t         flowlog_intern(flowlog_engine*, const uint8_t* text, size_t length);
//! int             flowlog_symbol(flowlog_engine*, int64_t id, const uint8_t** text, size_t* length);
//! flowlog_result* flowlog_evaluate(flowlog_engine*, const char* request_json,
//!                                  const flowlog_input* inputs, size_t input_count);
//! int             flowlog_result_ok(const flowlog_result*);
//! char*           flowlog_result_json(const flowlog_result*);
//! size_t          flowlog_result_relation_count(const flowlog_result*);
//! int             flowlog_result_relation(const flowlog_result*, size_t index, const char** name,
//!                                         size_t* arity, size_t* rows, const int64_t** cells,
//!                                         const char** types);
//! void            flowlog_result_free(flowlog_result*);
//! void            flowlog_string_free(char*);
//! ```
//!
//! `request_json` is `{"program": {"text": ..., "name": ...}, "options": {...}}`
//! with the same options as the service's `evaluate`. `flowlog_result_json`
//! carries `ok`, `stats`, `witnesses` and, on failure, `diagnostic`. A
//! relation's `types` is a string of one letter per column, `n` for number
//! and `s` for symbol.

use crate::accounting::Limits;
use crate::engine::{
    Engine, EngineConfig, EvaluationOptions, EvaluationRequest, EvaluationResult, Inputs,
    ProgramSource, Schedule,
};
use parsing::decl::DataType;
use parsing::diagnostic::Diagnostic;
use parsing::Val;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::ffi::{c_char, c_int, CStr, CString};
use std::sync::Arc;
use std::time::Duration;

pub struct FlowlogEngine {
    engine: Arc<Engine>,
}

#[repr(C)]
pub struct FlowlogInput {
    pub relation: *const c_char,
    pub arity: usize,
    pub cells: *const i64,
    pub rows: usize,
}

struct ResultRelation {
    name: CString,
    types: CString,
    arity: usize,
    rows: usize,
    cells: Vec<i64>,
}

pub struct FlowlogResult {
    outcome: Result<EvaluationResult, Diagnostic>,
    relations: Vec<ResultRelation>,
}

#[derive(Debug, Deserialize)]
struct RequestJson {
    program: ProgramJson,
    #[serde(default)]
    options: OptionsJson,
}

#[derive(Debug, Deserialize)]
struct ProgramJson {
    text: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OptionsJson {
    #[serde(default)]
    budget_seconds: Option<f64>,
    #[serde(default)]
    memory_limit_bytes: Option<u64>,
    #[serde(default)]
    tuple_limit: Option<u64>,
    #[serde(default)]
    cache: Option<bool>,
    #[serde(default)]
    schedule: Option<Schedule>,
    #[serde(default)]
    explain: Vec<(String, Vec<Val>)>,
    #[serde(default)]
    explain_all: bool,
}

fn json_string(value: &serde_json::Value) -> *mut c_char {
    CString::new(value.to_string())
        .unwrap_or_else(|_| CString::new("{}").expect("empty object"))
        .into_raw()
}

fn diagnostic_json(diagnostic: &Diagnostic) -> serde_json::Value {
    serde_json::json!({ "ok": false, "diagnostic": diagnostic })
}

unsafe fn text_of<'a>(pointer: *const c_char) -> Result<&'a str, Diagnostic> {
    if pointer.is_null() {
        return Err(Diagnostic::validation("a required text argument is null"));
    }
    CStr::from_ptr(pointer)
        .to_str()
        .map_err(|error| Diagnostic::validation(format!("an argument is not UTF-8: {error}")))
}

/// Create an engine from a JSON configuration (`{}` for the defaults). On
/// failure returns null and, when `error_json` is not null, a diagnostic.
#[no_mangle]
pub unsafe extern "C" fn flowlog_engine_new(
    config_json: *const c_char,
    error_json: *mut *mut c_char,
) -> *mut FlowlogEngine {
    let config = if config_json.is_null() {
        Ok(EngineConfig::default())
    } else {
        text_of(config_json).and_then(EngineConfig::from_json)
    };
    match config {
        Ok(config) => Box::into_raw(Box::new(FlowlogEngine {
            engine: Engine::new(config),
        })),
        Err(diagnostic) => {
            if !error_json.is_null() {
                *error_json = json_string(&diagnostic_json(&diagnostic));
            }
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn flowlog_engine_free(engine: *mut FlowlogEngine) {
    if !engine.is_null() {
        drop(Box::from_raw(engine));
    }
}

/// Check a program. Returns a JSON string: `{"ok": true, "report": ...}` or
/// `{"ok": false, "diagnostic": ...}`.
#[no_mangle]
pub unsafe extern "C" fn flowlog_check(
    engine: *const FlowlogEngine,
    program: *const c_char,
    name: *const c_char,
) -> *mut c_char {
    let outcome = (|| {
        let engine = engine
            .as_ref()
            .ok_or_else(|| Diagnostic::validation("the engine pointer is null"))?;
        let program = text_of(program)?;
        let name = if name.is_null() { "(host)" } else { text_of(name)? };
        engine.engine.check(program, name)
    })();
    json_string(&match outcome {
        Ok(report) => serde_json::json!({ "ok": true, "report": report }),
        Err(diagnostic) => diagnostic_json(&diagnostic),
    })
}

/// The id of a text, interning it; -1 when the text is not UTF-8 or the
/// engine refuses it.
#[no_mangle]
pub unsafe extern "C" fn flowlog_intern(
    engine: *const FlowlogEngine,
    text: *const u8,
    length: usize,
) -> i64 {
    let Some(engine) = engine.as_ref() else {
        return -1;
    };
    if length != 0 && text.is_null() {
        return -1;
    }
    let bytes = if length == 0 {
        &[][..]
    } else {
        std::slice::from_raw_parts(text, length)
    };
    match std::str::from_utf8(bytes) {
        Ok(text) => engine.engine.intern(text).unwrap_or(-1),
        Err(_) => -1,
    }
}

/// The text of a symbol id: 0 and the bytes (valid for the engine's life)
/// when known, 1 when not.
#[no_mangle]
pub unsafe extern "C" fn flowlog_symbol(
    engine: *const FlowlogEngine,
    id: i64,
    text: *mut *const u8,
    length: *mut usize,
) -> c_int {
    let Some(engine) = engine.as_ref() else {
        return 1;
    };
    match engine.engine.symbols().resolve_raw(id) {
        Some((pointer, size)) => {
            if !text.is_null() {
                *text = pointer;
            }
            if !length.is_null() {
                *length = size;
            }
            0
        }
        None => 1,
    }
}

/// Evaluate a program over flat inputs. Never returns null; ask the result
/// whether it is ok.
#[no_mangle]
pub unsafe extern "C" fn flowlog_evaluate(
    engine: *const FlowlogEngine,
    request_json: *const c_char,
    inputs: *const FlowlogInput,
    input_count: usize,
) -> *mut FlowlogResult {
    let outcome = (|| {
        let engine = engine
            .as_ref()
            .ok_or_else(|| Diagnostic::validation("the engine pointer is null"))?;
        let request: RequestJson = serde_json::from_str(text_of(request_json)?)
            .map_err(|error| Diagnostic::validation(format!("invalid evaluation request: {error}")))?;
        let mut rows = BTreeMap::new();
        if input_count != 0 && inputs.is_null() {
            return Err(Diagnostic::validation("the inputs pointer is null"));
        }
        for index in 0..input_count {
            let input = &*inputs.add(index);
            let relation = text_of(input.relation)?.to_string();
            let count = input.arity * input.rows;
            if count != 0 && input.cells.is_null() {
                return Err(Diagnostic::input(format!("the cells of {relation} are null")));
            }
            let cells = if count == 0 {
                &[][..]
            } else {
                std::slice::from_raw_parts(input.cells, count)
            };
            let relation_rows = if input.arity == 0 {
                vec![Vec::new(); input.rows.min(1)]
            } else {
                cells.chunks_exact(input.arity).map(<[Val]>::to_vec).collect()
            };
            rows.insert(relation, relation_rows);
        }
        let options = request.options;
        let engine_request = EvaluationRequest {
            program: ProgramSource::Text {
                name: request.program.name.unwrap_or_else(|| "(host)".to_string()),
                source: request.program.text,
            },
            inputs: Inputs::Rows(rows),
            options: EvaluationOptions {
                limits: Limits {
                    time: options.budget_seconds.map(Duration::from_secs_f64),
                    cancel: None,
                    memory_bytes: options.memory_limit_bytes,
                    tuples: options.tuple_limit,
                },
                cache: options.cache,
                schedule: options.schedule,
                explain: options.explain,
                explain_all: options.explain_all,
            },
        };
        engine.engine.evaluate(engine_request)
    })();

    let relations = match &outcome {
        Ok(result) => result
            .outputs
            .iter()
            .filter_map(|(name, state)| {
                let types = result.types.get(name)?;
                let letters = types
                    .iter()
                    .map(|column| match column {
                        DataType::Integer => 'n',
                        DataType::Symbol => 's',
                    })
                    .collect::<String>();
                Some(ResultRelation {
                    name: CString::new(name.as_str()).ok()?,
                    types: CString::new(letters).ok()?,
                    arity: state.arity,
                    rows: state.rows.len(),
                    cells: state.rows.iter().flat_map(|row| row.iter().copied()).collect(),
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    Box::into_raw(Box::new(FlowlogResult { outcome, relations }))
}

#[no_mangle]
pub unsafe extern "C" fn flowlog_result_ok(result: *const FlowlogResult) -> c_int {
    match result.as_ref() {
        Some(result) if result.outcome.is_ok() => 1,
        _ => 0,
    }
}

/// The result's counters, witnesses and diagnostic as JSON.
#[no_mangle]
pub unsafe extern "C" fn flowlog_result_json(result: *const FlowlogResult) -> *mut c_char {
    let Some(result) = result.as_ref() else {
        return json_string(&diagnostic_json(&Diagnostic::validation("the result pointer is null")));
    };
    json_string(&match &result.outcome {
        Ok(result) => serde_json::json!({
            "ok": true,
            "program": result.program_name,
            "stats": result.stats,
            "witnesses": result.witnesses,
            "declared_outputs": result.declared_outputs,
        }),
        Err(diagnostic) => diagnostic_json(diagnostic),
    })
}

#[no_mangle]
pub unsafe extern "C" fn flowlog_result_relation_count(result: *const FlowlogResult) -> usize {
    result.as_ref().map_or(0, |result| result.relations.len())
}

/// The `index`th relation of a result: 0 and the fields when it exists, 1
/// otherwise. `cells` holds `rows * arity` values, row-major, valid until the
/// result is freed.
#[no_mangle]
pub unsafe extern "C" fn flowlog_result_relation(
    result: *const FlowlogResult,
    index: usize,
    name: *mut *const c_char,
    arity: *mut usize,
    rows: *mut usize,
    cells: *mut *const i64,
    types: *mut *const c_char,
) -> c_int {
    let Some(relation) = result.as_ref().and_then(|result| result.relations.get(index)) else {
        return 1;
    };
    if !name.is_null() {
        *name = relation.name.as_ptr();
    }
    if !arity.is_null() {
        *arity = relation.arity;
    }
    if !rows.is_null() {
        *rows = relation.rows;
    }
    if !cells.is_null() {
        *cells = relation.cells.as_ptr();
    }
    if !types.is_null() {
        *types = relation.types.as_ptr();
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn flowlog_result_free(result: *mut FlowlogResult) {
    if !result.is_null() {
        drop(Box::from_raw(result));
    }
}

#[no_mangle]
pub unsafe extern "C" fn flowlog_string_free(text: *mut c_char) {
    if !text.is_null() {
        drop(CString::from_raw(text));
    }
}

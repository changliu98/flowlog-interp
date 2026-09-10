//! The engine's contract, exercised end to end: the dialect, the value domain,
//! embedded functions, diagnostics, limits, explanations, the service and the
//! C interface.

use flowlog::accounting::{CancelToken, Limits};
use flowlog::capi;
use flowlog::daemon::{request, serve, DaemonRequest, InputSpec, OptionsSpec, OutputSpec, ProgramSpec, ServiceDefaults};
use flowlog::engine::{
    Engine, EngineConfig, EvaluationOptions, EvaluationRequest, EvaluationResult, Inputs,
    ProgramSource, Schedule,
};
use parsing::diagnostic::DiagnosticKind;
use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct TempTree(PathBuf);

impl TempTree {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "flowlog-features-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn engine() -> Arc<Engine> {
    Engine::new(EngineConfig {
        workers: 2,
        cache_memory_bytes: 64 * 1024 * 1024,
        ..EngineConfig::default()
    })
}

fn rows(relation: &str, rows: &[&[i64]]) -> (String, Vec<Vec<i64>>) {
    (
        relation.to_string(),
        rows.iter().map(|row| row.to_vec()).collect(),
    )
}

fn evaluate(
    engine: &Engine,
    source: &str,
    inputs: Vec<(String, Vec<Vec<i64>>)>,
    options: EvaluationOptions,
) -> Result<EvaluationResult, parsing::diagnostic::Diagnostic> {
    engine.evaluate(EvaluationRequest {
        program: ProgramSource::Text {
            name: "features.dl".to_string(),
            source: source.to_string(),
        },
        inputs: Inputs::Rows(inputs.into_iter().collect::<BTreeMap<_, _>>()),
        options,
    })
}

fn sorted(result: &EvaluationResult, relation: &str) -> Vec<Vec<i64>> {
    result.outputs[relation].rows.iter().cloned().collect()
}

#[test]
fn aggregates_may_sit_in_any_column_and_take_expressions() {
    let engine = engine();
    let result = evaluate(
        &engine,
        ".in\n.decl E(k: number, v: number)\n.printsize\n\
         .decl First(m: number, k: number)\n\
         .decl Middle(k: number, s: number, j: number)\n\
         .decl Counted(k: number, c: number)\n\
         .decl Summed(k: number, s: number)\n\
         .rule\n\
         First(min(v), k) :- E(k, v).\n\
         Middle(k, max(v), 1) :- E(k, v).\n\
         Counted(k, count(v)) :- E(k, v).\n\
         Summed(k, sum(v * 10 + 1)) :- E(k, v).\n",
        vec![rows("E", &[&[1, 3], &[1, 5], &[2, -4], &[2, 7]])],
        EvaluationOptions::default(),
    )
    .unwrap();
    assert_eq!(sorted(&result, "First"), vec![vec![-4, 2], vec![3, 1]]);
    assert_eq!(sorted(&result, "Middle"), vec![vec![1, 5, 1], vec![2, 7, 1]]);
    assert_eq!(sorted(&result, "Counted"), vec![vec![1, 2], vec![2, 2]]);
    // (v * 10) + 1 summed: 1 -> 31 + 51, 2 -> -39 + 71
    assert_eq!(sorted(&result, "Summed"), vec![vec![1, 82], vec![2, 32]]);
}

#[test]
fn an_arity_zero_relation_says_whether_anything_holds() {
    let engine = engine();
    let program = ".in\n.decl E(k: number)\n.printsize\n.decl Any()\n.decl Marked(k: number)\n\
                   .rule\nAny() :- E(k), k > 5.\nMarked(k) :- E(k), Any().\n";
    let result = evaluate(
        &engine,
        program,
        vec![rows("E", &[&[1], &[9]])],
        EvaluationOptions::default(),
    )
    .unwrap();
    assert_eq!(sorted(&result, "Any"), vec![Vec::<i64>::new()]);
    assert_eq!(sorted(&result, "Marked"), vec![vec![1], vec![9]]);

    let empty = evaluate(
        &engine,
        program,
        vec![rows("E", &[&[1]])],
        EvaluationOptions::default(),
    )
    .unwrap();
    assert_eq!(sorted(&empty, "Any"), Vec::<Vec<i64>>::new());
    assert_eq!(sorted(&empty, "Marked"), Vec::<Vec<i64>>::new());
}

#[test]
fn symbols_flow_through_functions_constants_and_outputs() {
    let engine = engine();
    let main = engine.intern("main").unwrap();
    let helper = engine.intern("helper").unwrap();
    let result = evaluate(
        &engine,
        ".code rust\n\
         pub fn suffixed(name: Symbol) -> Symbol { Symbol::new(&format!(\"{}_1\", name.as_str())) }\n\
         pub fn is_main(name: Symbol) -> bool { name.as_str() == \"main\" }\n\
         pub fn length(name: Symbol) -> i64 { name.as_str().len() as i64 }\n\
         .endcode\n\
         .in\n.decl Fn(name: symbol, k: number)\n.printsize\n\
         .decl Renamed(name: symbol, k: number)\n\
         .decl Main(k: number)\n\
         .decl Lengths(name: symbol, n: number)\n\
         .decl Literal(k: number)\n\
         .rule\n\
         Renamed(r, k) :- Fn(n, k), r = @call(suffixed, n).\n\
         Main(k) :- Fn(n, k), @call(is_main, n).\n\
         Lengths(n, l) :- Fn(n, k), l = @call(length, n).\n\
         Literal(k) :- Fn(\"helper\", k).\n",
        vec![rows("Fn", &[&[main, 1], &[helper, 2]])],
        EvaluationOptions::default(),
    )
    .unwrap();
    let main_1 = engine.symbol_text(flowlog::symbols::symbol_id("main_1"));
    assert_eq!(main_1.as_deref(), Some("main_1"), "the function interned its result");
    assert_eq!(
        sorted(&result, "Renamed"),
        {
            let mut expected = vec![
                vec![flowlog::symbols::symbol_id("main_1"), 1],
                vec![flowlog::symbols::symbol_id("helper_1"), 2],
            ];
            expected.sort();
            expected
        }
    );
    assert_eq!(sorted(&result, "Main"), vec![vec![1]]);
    let mut lengths = sorted(&result, "Lengths");
    lengths.sort();
    let mut expected = vec![vec![main, 4], vec![helper, 6]];
    expected.sort();
    assert_eq!(lengths, expected);
    assert_eq!(sorted(&result, "Literal"), vec![vec![2]]);
    assert_eq!(
        result.rows_json(&engine, "Main").unwrap(),
        serde_json::json!([[1]])
    );
}

#[test]
fn a_function_panic_is_a_diagnostic_naming_the_function_and_its_line() {
    let engine = engine();
    let error = evaluate(
        &engine,
        ".code rust\n\
         pub fn checked(x: i64) -> i64 {\n\
             if x > 1 { panic!(\"too large: {}\", x) }\n\
             x\n\
         }\n\
         .endcode\n\
         .in\n.decl E(k: number)\n.printsize\n.decl R(k: number)\n.rule\n\
         R(y) :- E(x), y = @call(checked, x).\n",
        vec![rows("E", &[&[1], &[2]])],
        EvaluationOptions::default(),
    )
    .unwrap_err();
    assert_eq!(error.kind, DiagnosticKind::Function, "{error}");
    assert_eq!(error.function.as_deref(), Some("checked"));
    assert!(error.message.contains("line 2 of its block"), "{error}");
    assert!(error.message.contains("too large: 2"), "{error}");
    // program line: the block starts on line 2 of the program
    assert_eq!(error.location.as_ref().unwrap().line, 3);
    assert!(error.rule.as_deref().unwrap().contains("@call(checked, x)"), "{error}");

    // the engine is fine afterwards
    let result = evaluate(
        &engine,
        ".in\n.decl E(k: number)\n.printsize\n.decl R(k: number)\n.rule\nR(k) :- E(k).\n",
        vec![rows("E", &[&[1]])],
        EvaluationOptions::default(),
    )
    .unwrap();
    assert_eq!(sorted(&result, "R"), vec![vec![1]]);
}

#[test]
fn a_block_that_does_not_compile_is_reported_on_its_own_lines() {
    let engine = engine();
    let error = evaluate(
        &engine,
        ".in\n.decl E(k: number)\n.printsize\n.decl R(k: number)\n\
         .code rust\n\
         pub fn broken(x: i64) -> i64 {\n\
             let narrow: u8 = x;\n\
             narrow as i64\n\
         }\n\
         .endcode\n\
         .rule\nR(y) :- E(x), y = @call(broken, x).\n",
        vec![rows("E", &[&[1]])],
        EvaluationOptions::default(),
    )
    .unwrap_err();
    assert_eq!(error.kind, DiagnosticKind::Function, "{error}");
    let detail = error.detail.as_deref().unwrap_or("");
    assert!(detail.contains("line 2 of the block"), "{error}");
    assert!(detail.contains("mismatched types"), "{error}");
    // the block's first line is program line 6, so its line 2 is line 7
    assert_eq!(error.location.as_ref().unwrap().line, 7, "{error}");

    // a block that is not Rust at all is refused when the program is parsed
    let error = evaluate(
        &engine,
        ".code rust\npub fn broken(x: i64) -> i64 {\n    x +\n}\n.endcode\n\
         .in\n.decl E(k: number)\n.printsize\n.decl R(k: number)\n.rule\nR(y) :- E(x), y = @call(broken, x).\n",
        vec![rows("E", &[&[1]])],
        EvaluationOptions::default(),
    )
    .unwrap_err();
    assert_eq!(error.kind, DiagnosticKind::Function, "{error}");
    assert!(error.message.contains("invalid embedded Rust"), "{error}");
    assert_eq!(error.location.as_ref().unwrap().line, 4, "{error}");
}

#[test]
fn division_by_zero_is_an_evaluation_diagnostic() {
    let engine = engine();
    let error = evaluate(
        &engine,
        ".in\n.decl E(k: number, v: number)\n.printsize\n.decl R(k: number)\n.rule\n\
         R(k / v) :- E(k, v).\n",
        vec![rows("E", &[&[4, 2], &[1, 0]])],
        EvaluationOptions::default(),
    )
    .unwrap_err();
    assert_eq!(error.kind, DiagnosticKind::Evaluation, "{error}");
    assert!(error.message.contains("division by zero"), "{error}");
    assert!(error.rule.is_some(), "{error}");
}

#[test]
fn a_time_budget_stops_a_runaway_recursion() {
    let engine = engine();
    let started = std::time::Instant::now();
    let error = evaluate(
        &engine,
        ".in\n.decl Seed(k: number)\n.printsize\n.decl N(k: number)\n.rule\n\
         N(k) :- Seed(k).\n\
         N(k + 1) :- N(k), k < 1000000000.\n",
        vec![rows("Seed", &[&[0]])],
        EvaluationOptions {
            limits: Limits {
                time: Some(Duration::from_millis(300)),
                ..Limits::default()
            },
            ..EvaluationOptions::default()
        },
    )
    .unwrap_err();
    assert_eq!(error.kind, DiagnosticKind::Resource, "{error}");
    assert!(error.message.contains("time budget"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(30), "the evaluation did not stop");
}

#[test]
fn a_cancellation_and_a_tuple_ceiling_are_resource_diagnostics() {
    let engine = engine();
    let cancel = CancelToken::new();
    cancel.cancel();
    let error = evaluate(
        &engine,
        ".in\n.decl E(k: number)\n.printsize\n.decl R(k: number)\n.rule\nR(k) :- E(k).\n",
        vec![rows("E", &[&[1]])],
        EvaluationOptions {
            limits: Limits {
                cancel: Some(cancel),
                ..Limits::default()
            },
            ..EvaluationOptions::default()
        },
    )
    .unwrap_err();
    assert_eq!(error.kind, DiagnosticKind::Resource, "{error}");
    assert!(error.message.contains("cancelled"), "{error}");

    let error = evaluate(
        &engine,
        ".in\n.decl E(k: number)\n.printsize\n.decl R(k: number)\n.rule\nR(k) :- E(k).\n",
        vec![rows("E", &[&[1], &[2], &[3]])],
        EvaluationOptions {
            limits: Limits {
                tuples: Some(2),
                ..Limits::default()
            },
            ..EvaluationOptions::default()
        },
    )
    .unwrap_err();
    assert!(error.message.contains("tuple ceiling"), "{error}");
}

#[test]
fn a_witness_names_the_rule_and_the_parents() {
    let engine = engine();
    let result = evaluate(
        &engine,
        ".in\n.decl Edge(x: number, y: number)\n.printsize\n.decl Reach(x: number, y: number)\n\
         .decl Count(x: number, c: number)\n.rule\n\
         Reach(x, y) :- Edge(x, y).\n\
         Reach(x, z) :- Reach(x, y), Edge(y, z).\n\
         Count(x, count(y)) :- Reach(x, y).\n",
        vec![rows("Edge", &[&[1, 2], &[2, 3]])],
        EvaluationOptions {
            explain: vec![
                ("Reach".to_string(), vec![1, 3]),
                ("Count".to_string(), vec![1, 2]),
                ("Reach".to_string(), vec![3, 1]),
                ("Edge".to_string(), vec![1, 2]),
            ],
            ..EvaluationOptions::default()
        },
    )
    .unwrap();
    let [reach, count, absent, input] = &result.witnesses[..] else {
        panic!("four witnesses, got {}", result.witnesses.len());
    };
    assert_eq!(reach.rule, Some(1), "{reach:?}");
    assert_eq!(reach.line, Some(8));
    assert_eq!(
        reach.parents.iter().map(|parent| (parent.relation.as_str(), parent.row.clone())).collect::<Vec<_>>(),
        vec![("Reach", vec![1, 2]), ("Edge", vec![2, 3])]
    );
    assert_eq!(count.rule, Some(2));
    assert_eq!(count.group_size, Some(2));
    assert_eq!(count.parents.len(), 2);
    assert!(!absent.present);
    assert!(input.present && input.input && input.rule.is_none());
}

#[test]
fn true_and_false_in_bodies_and_several_blocks_are_accepted() {
    let engine = engine();
    let result = evaluate(
        &engine,
        ".code rust\npub fn one(x: i64) -> i64 { x + 1 }\n.endcode\n\
         .code rust\nfn helper(x: i64) -> i64 { x * 2 }\npub fn two(x: i64) -> i64 { helper(x) }\n.endcode\n\
         .in\n.decl E(k: number)\n.printsize\n.decl R(a: number, b: number)\n.decl Never(k: number)\n.rule\n\
         R(a, b) :- E(k), a = @call(one, k), b = @call(two, a), True.\n\
         Never(k) :- E(k), False.\n",
        vec![rows("E", &[&[1]])],
        EvaluationOptions::default(),
    )
    .unwrap();
    assert_eq!(sorted(&result, "R"), vec![vec![2, 4]]);
    assert_eq!(sorted(&result, "Never"), Vec::<Vec<i64>>::new());
}

#[test]
fn a_comparison_over_a_call_result_and_a_computed_head_agree_with_the_whole_program_schedule() {
    let engine = engine();
    let program = ".code rust\npub fn twice(x: i64) -> i64 { x * 2 }\n.endcode\n\
                   .in\n.decl E(k: number, v: number)\n.printsize\n.decl R(a: number, b: number)\n.rule\n\
                   R(d, k + v) :- E(k, v), d = @call(twice, k), d > v + 1.\n";
    let cached = evaluate(
        &engine,
        program,
        vec![rows("E", &[&[1, 0], &[1, 5], &[3, 2]])],
        EvaluationOptions::default(),
    )
    .unwrap();
    let whole = evaluate(
        &engine,
        program,
        vec![rows("E", &[&[1, 0], &[1, 5], &[3, 2]])],
        EvaluationOptions {
            cache: Some(false),
            schedule: Some(Schedule::WholeProgram),
            ..EvaluationOptions::default()
        },
    )
    .unwrap();
    assert_eq!(sorted(&cached, "R"), vec![vec![2, 1], vec![6, 5]]);
    assert_eq!(sorted(&cached, "R"), sorted(&whole, "R"));
}

#[test]
fn check_reports_the_program_without_evaluating_it() {
    let engine = engine();
    let report = engine
        .check(
            ".code rust\npub fn f(x: i64) -> i64 { x }\n.endcode\n\
             .in\n.decl E(k: number, s: symbol)\n.printsize\n.decl R(k: number)\n.rule\n\
             H(s, k) :- E(k, s).\nR(k) :- H(s, k).\nR(k) :- R(j), E(k, s), j < k.\n",
            "checked.dl",
        )
        .unwrap();
    assert_eq!(report.rules, 3);
    // H; R from H; R recursively
    assert_eq!(report.strata, 3);
    assert_eq!(report.recursive_strata, 1);
    assert_eq!(report.functions, vec!["fn f(i64) -> i64".to_string()]);
    let h = report.relations.iter().find(|relation| relation.name == "H").unwrap();
    assert!(h.derived && !h.input && !h.output);
    assert_eq!(h.columns, vec![parsing::decl::DataType::Symbol, parsing::decl::DataType::Integer]);

    let error = engine
        .check(".in\n.decl E(k: number)\n.printsize\n.decl R(k: number)\n.rule\nR(k) :- E(k), !R(k).\n", "loop.dl")
        .unwrap_err();
    assert_eq!(error.kind, DiagnosticKind::Stratification);
    assert!(error.message.contains("not stratifiable"), "{error}");
}

#[test]
fn the_command_line_reports_a_json_diagnostic_and_checks_programs() {
    let temp = TempTree::new("cli");
    let program = temp.path("program.dl");
    fs::write(
        &program,
        ".in\n.decl E(k: number)\n.input E.facts\n.printsize\n.decl R(k: number)\n.rule\nR(j) :- E(k).\n",
    )
    .unwrap();
    let facts = temp.path("facts");
    fs::create_dir_all(&facts).unwrap();
    fs::write(facts.join("E.facts"), "1\n").unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_executing"))
        .arg("--program")
        .arg(&program)
        .arg("--facts")
        .arg(&facts)
        .env("FLOWLOG_DIAGNOSTIC_JSON", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last = stderr.lines().last().unwrap();
    let diagnostic: serde_json::Value = serde_json::from_str(last).unwrap_or_else(|_| panic!("{stderr}"));
    assert_eq!(diagnostic["kind"], "validation");
    assert_eq!(diagnostic["location"]["line"], 7);

    fs::write(
        &program,
        ".in\n.decl E(k: number)\n.input E.facts\n.printsize\n.decl R(k: number)\n.rule\nR(k) :- E(k).\n",
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_executing"))
        .arg("--program")
        .arg(&program)
        .arg("--check")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["rules"], 1);

    let csvs = temp.path("out");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_executing"))
        .arg("--program")
        .arg(&program)
        .arg("--facts")
        .arg(&facts)
        .arg("--csvs")
        .arg(&csvs)
        .arg("--explain")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let explained = fs::read_to_string(csvs.join("csvs/explain.jsonl")).unwrap();
    assert_eq!(explained.lines().count(), 1);
    assert!(explained.contains("\"rule\":0"), "{explained}");
}

#[test]
fn the_service_evaluates_inline_rows_with_symbols_and_reports_diagnostics() {
    let temp = TempTree::new("service");
    let socket = temp.path("flowlog.sock");
    let engine = engine();
    let defaults = ServiceDefaults {
        program: None,
        facts: None,
        csvs: None,
    };
    let daemon_socket = socket.clone();
    let daemon = std::thread::spawn(move || serve(engine, defaults, daemon_socket));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !socket.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(socket.exists());

    let program = ".in\n.decl Fn(name: symbol, k: number)\n.printsize\n.decl Out(name: symbol, k: number)\n.rule\n\
                   Out(n, k + 1) :- Fn(n, k), n = \"main\".\n";
    let reply = request(
        &socket,
        DaemonRequest::Evaluate {
            id: Some("one".to_string()),
            program: ProgramSpec {
                path: None,
                text: Some(program.to_string()),
                name: Some("inline.dl".to_string()),
            },
            inputs: InputSpec {
                facts: None,
                rows: Some(BTreeMap::from([(
                    "Fn".to_string(),
                    vec![
                        vec![serde_json::json!("main"), serde_json::json!(1)],
                        vec![serde_json::json!("other"), serde_json::json!(2)],
                    ],
                )])),
            },
            output: OutputSpec {
                csvs: None,
                inline: true,
            },
            options: OptionsSpec {
                explain_all: true,
                ..OptionsSpec::default()
            },
        },
    )
    .unwrap();
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["id"], "one");
    assert_eq!(reply["outputs"]["Out"], serde_json::json!([["main", 2]]));
    assert_eq!(reply["types"]["Out"], serde_json::json!(["symbol", "number"]));
    assert_eq!(reply["witnesses"].as_array().unwrap().len(), 1);

    let check = request(
        &socket,
        DaemonRequest::Check {
            program: ProgramSpec {
                path: None,
                text: Some(".in\n.decl E(k: number)\n.printsize\n.decl R(k: number)\n.rule\nR(j) :- E(k).\n".to_string()),
                name: Some("bad.dl".to_string()),
            },
        },
    )
    .unwrap();
    let check: serde_json::Value = serde_json::from_str(&check).unwrap();
    assert_eq!(check["ok"], false);
    assert_eq!(check["diagnostic"]["kind"], "validation", "{check}");

    let missing = request(&socket, DaemonRequest::Cancel { id: "nope".to_string() }).unwrap();
    let missing: serde_json::Value = serde_json::from_str(&missing).unwrap();
    assert_eq!(missing["ok"], false);

    let shutdown = request(&socket, DaemonRequest::Shutdown).unwrap();
    let shutdown: serde_json::Value = serde_json::from_str(&shutdown).unwrap();
    assert_eq!(shutdown["ok"], true);
    daemon.join().unwrap().unwrap();
}

#[test]
fn the_c_interface_round_trips_cells_and_diagnostics() {
    unsafe {
        let mut error: *mut std::ffi::c_char = std::ptr::null_mut();
        let config = CString::new("{\"workers\": 1}").unwrap();
        let engine = capi::flowlog_engine_new(config.as_ptr(), &mut error);
        assert!(!engine.is_null());
        assert!(error.is_null());

        let main = capi::flowlog_intern(engine, b"main".as_ptr(), 4);
        assert_eq!(main, flowlog::symbols::symbol_id("main"));
        let mut pointer: *const u8 = std::ptr::null();
        let mut length = 0usize;
        assert_eq!(capi::flowlog_symbol(engine, main, &mut pointer, &mut length), 0);
        assert_eq!(std::slice::from_raw_parts(pointer, length), b"main");

        let program = CString::new(
            ".in\n.decl Fn(name: symbol, k: number)\n.printsize\n.decl Out(k: number)\n.rule\n\
             Out(k * 2) :- Fn(n, k), n = \"main\".\n",
        )
        .unwrap();
        let name = CString::new("capi.dl").unwrap();
        let checked = capi::flowlog_check(engine, program.as_ptr(), name.as_ptr());
        let report: serde_json::Value = serde_json::from_str(CStr::from_ptr(checked).to_str().unwrap()).unwrap();
        assert_eq!(report["ok"], true, "{report}");
        capi::flowlog_string_free(checked);

        let request = CString::new(format!(
            "{{\"program\": {{\"text\": {}, \"name\": \"capi.dl\"}}, \"options\": {{\"explain_all\": true}}}}",
            serde_json::to_string(program.to_str().unwrap()).unwrap()
        ))
        .unwrap();
        let relation = CString::new("Fn").unwrap();
        let cells: Vec<i64> = vec![main, 3, capi::flowlog_intern(engine, b"other".as_ptr(), 5), 4];
        let inputs = [capi::FlowlogInput {
            relation: relation.as_ptr(),
            arity: 2,
            cells: cells.as_ptr(),
            rows: 2,
        }];
        let result = capi::flowlog_evaluate(engine, request.as_ptr(), inputs.as_ptr(), 1);
        assert_eq!(capi::flowlog_result_ok(result), 1);
        let json = capi::flowlog_result_json(result);
        let summary: serde_json::Value = serde_json::from_str(CStr::from_ptr(json).to_str().unwrap()).unwrap();
        assert_eq!(summary["ok"], true, "{summary}");
        assert_eq!(summary["witnesses"].as_array().unwrap().len(), 1);
        capi::flowlog_string_free(json);

        let count = capi::flowlog_result_relation_count(result);
        let mut found = false;
        for index in 0..count {
            let mut name: *const std::ffi::c_char = std::ptr::null();
            let mut arity = 0usize;
            let mut rows = 0usize;
            let mut cells: *const i64 = std::ptr::null();
            let mut types: *const std::ffi::c_char = std::ptr::null();
            assert_eq!(
                capi::flowlog_result_relation(result, index, &mut name, &mut arity, &mut rows, &mut cells, &mut types),
                0
            );
            if CStr::from_ptr(name).to_str().unwrap() == "Out" {
                found = true;
                assert_eq!((arity, rows), (1, 1));
                assert_eq!(*cells, 6);
                assert_eq!(CStr::from_ptr(types).to_str().unwrap(), "n");
            }
        }
        assert!(found);
        capi::flowlog_result_free(result);

        let bad = CString::new("{\"program\": {\"text\": \".in\\n.decl E(k: number)\\n.rule\\nR(j) :- E(k).\\n\"}}").unwrap();
        let result = capi::flowlog_evaluate(engine, bad.as_ptr(), std::ptr::null(), 0);
        assert_eq!(capi::flowlog_result_ok(result), 0);
        let json = capi::flowlog_result_json(result);
        let failure: serde_json::Value = serde_json::from_str(CStr::from_ptr(json).to_str().unwrap()).unwrap();
        assert_eq!(failure["ok"], false);
        assert_eq!(failure["diagnostic"]["kind"], "validation", "{failure}");
        capi::flowlog_string_free(json);
        capi::flowlog_result_free(result);
        capi::flowlog_engine_free(engine);
    }
}

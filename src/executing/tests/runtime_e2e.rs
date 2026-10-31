//! End-to-end behaviour of the runtime around one evaluation: parallel input
//! preparation, programs whose heads span strata, sideways information
//! passing, and the service evaluating named paths concurrently.

use flowlog::daemon::{request, serve, DaemonRequest, ServiceDefaults};
use flowlog::engine::{Engine, EngineConfig, EvaluationOptions, EvaluationRequest, Inputs, ProgramSource};
use flowlog::files;
use parsing::parser::Program;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

struct TempTree(PathBuf);

impl TempTree {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should follow Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "flowlog-interp-runtime-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create fixture directory");
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

fn reach_program(mark_rule: &str) -> String {
    format!(
        ".in\n\
         .decl Edge(x: number, y: number)\n\
         .input Edge.csv\n\
         .printsize\n\
         .decl Reach(x: number, y: number)\n\
         .decl Mark(x: number)\n\
         .rule\n\
         Reach(x, y) :- Edge(x, y).\n\
         Reach(x, z) :- Reach(x, y), Edge(y, z).\n\
         {mark_rule}\n"
    )
}

fn engine_with(adjust: impl FnOnce(&mut EngineConfig)) -> Arc<Engine> {
    let mut config = EngineConfig {
        workers: 2,
        ..EngineConfig::default()
    };
    adjust(&mut config);
    Engine::new(config)
}

/// Evaluate `program` over `facts`, writing outputs to `output`.
fn evaluate(engine: &Engine, program: &Path, facts: &Path, output: &Path) {
    let source = fs::read_to_string(program).unwrap();
    let name = program.to_string_lossy().to_string();
    let parsed = Program::parse(&source, &name).unwrap();
    let result = engine
        .evaluate(EvaluationRequest {
            program: ProgramSource::Text { name, source },
            inputs: Inputs::Directory(facts.to_path_buf()),
            options: EvaluationOptions::default(),
        })
        .unwrap_or_else(|diagnostic| panic!("{diagnostic}"));
    let states = result
        .outputs
        .iter()
        .map(|(name, state)| (name.clone(), Arc::clone(state)))
        .collect();
    files::write_outputs(&parsed, &states, output, b',', engine.symbols()).unwrap();
}

fn sorted_rows(path: &Path) -> Vec<String> {
    let mut rows = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    rows.sort();
    rows
}

fn strings(rows: &[&str]) -> Vec<String> {
    rows.iter().map(|row| row.to_string()).collect()
}

#[test]
fn parallel_file_preparation_preserves_states_and_refuses_bad_rows() {
    let temp = TempTree::new("parallel-input");
    let program = Program::parse(
        ".in\n.decl E(x: number, y: number)\n.input E.facts\n\
         .decl Z()\n.input Z.facts\n.decl S(s: symbol)\n.input S.facts\n\
         .printsize\n.decl O(x: number)\n.rule\nO(x) :- E(x,y).\n",
        "parallel-input.dl",
    ).unwrap();
    let mut contents = String::new();
    for value in (0..2051).rev() {
        let line = format!("{value},{}\r\n", value % 11);
        contents.push_str(&line);
        contents.push_str(&line);
    }
    fs::write(temp.path("E.facts"), &contents).unwrap();
    fs::write(temp.path("Z.facts"), "\n\r\n\n").unwrap();
    fs::write(temp.path("S.facts"), "zebra\n字\r\nalpha\n字").unwrap();
    let symbols = flowlog::symbols::SymbolTable::new();
    let serial = files::read_facts_directory(&program, &temp.0, b',', &symbols).unwrap();
    let parallel = files::read_facts_directory_with_workers(&program, &temp.0, b',', &symbols, 32).unwrap();
    assert_eq!(serial["E"].rows.len(), 2051);
    assert_eq!(serial["Z"].rows.len(), 1);
    assert_eq!(serial["S"].rows.len(), 3);
    for (name, state) in &serial {
        assert_eq!(state.rows, parallel[name].rows, "{name}");
    }
    contents.push_str("2052,bad\n");
    fs::write(temp.path("E.facts"), contents).unwrap();
    let error = files::read_facts_directory_with_workers(&program, &temp.0, b',', &symbols, 32).unwrap_err();
    assert!(error.message.contains("not a number"), "{error}");
}

#[test]
fn a_head_with_rules_in_two_strata_accumulates_across_them() {
    let temp = TempTree::new("two-strata-head");
    let facts = temp.path("facts");
    fs::create_dir_all(&facts).unwrap();
    fs::write(facts.join("A.facts"), "1\n2\n").unwrap();
    fs::write(facts.join("B.facts"), "2\n3\n").unwrap();
    let program_path = temp.path("program.dl");
    // H gets rows at the first stratum (from A) and again later (from G, which
    // is derived from B), so the second stratum reads H's earlier rows.
    fs::write(
        &program_path,
        ".in\n\
         .decl A(x: number)\n\
         .input A.facts\n\
         .decl B(x: number)\n\
         .input B.facts\n\
         .printsize\n\
         .decl G(x: number)\n\
         .decl H(x: number)\n\
         .decl K(x: number)\n\
         .rule\n\
         H(x) :- A(x).\n\
         G(x) :- B(x).\n\
         H(x) :- G(x).\n\
         K(x) :- H(x).\n",
    )
    .unwrap();
    let output = temp.path("output");
    evaluate(&engine_with(|_| {}), &program_path, &facts, &output);
    assert_eq!(sorted_rows(&output.join("csvs/H.csv")), strings(&["1", "2", "3"]));
    assert_eq!(sorted_rows(&output.join("csvs/K.csv")), strings(&["1", "2", "3"]));
}

#[test]
fn sideways_information_passing_reaches_the_fixed_point() {
    let temp = TempTree::new("sip");
    let facts = temp.path("facts");
    fs::create_dir_all(&facts).unwrap();
    fs::write(facts.join("A.facts"), "1,2\n2,3\n").unwrap();
    fs::write(facts.join("B.facts"), "3,4\n").unwrap();
    fs::write(facts.join("C.facts"), "7\n").unwrap();
    let program_path = temp.path("program.dl");
    fs::write(
        &program_path,
        ".in\n\
         .decl A(x: number, y: number)\n\
         .input A.facts\n\
         .decl B(x: number, y: number)\n\
         .input B.facts\n\
         .decl C(z: number)\n\
         .input C.facts\n\
         .printsize\n\
         .decl H(x: number, y: number, z: number)\n\
         .rule\n\
         H(x, y, z) :- A(x, y), C(z).\n\
         H(x, y, z) :- H(x, w, q), A(w, v), B(v, y), C(z).\n",
    )
    .unwrap();
    for opt_level in [None, Some(1), Some(3)] {
        let output = temp.path(&format!("output-{opt_level:?}"));
        evaluate(&engine_with(|config| config.opt_level = opt_level), &program_path, &facts, &output);
        assert_eq!(
            sorted_rows(&output.join("csvs/H.csv")),
            strings(&["1,2,7", "1,4,7", "2,3,7"]),
            "-O {opt_level:?}"
        );
    }
}

#[test]
fn the_service_serves_named_paths_concurrently_and_removes_its_socket() {
    let temp = TempTree::new("daemon");
    let facts = temp.path("facts");
    fs::create_dir_all(&facts).unwrap();
    fs::write(facts.join("Edge.csv"), "1,2\n2,3\n3,4\n").unwrap();
    let program_path = temp.path("program.dl");
    fs::write(&program_path, reach_program("Mark(x) :- Reach(1, x).")).unwrap();
    let output = temp.path("output");
    let socket = temp.path("flowlog.sock");

    let engine = engine_with(|_| {});
    let defaults = ServiceDefaults {
        program: Some(program_path.clone()),
        facts: Some(facts.clone()),
        csvs: Some(output.clone()),
    };
    let daemon_socket = socket.clone();
    let daemon = std::thread::spawn(move || serve(engine, defaults, daemon_socket));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !socket.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(socket.exists(), "daemon did not create its socket");

    let first = request(&socket, DaemonRequest::reload()).unwrap();
    let first: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(first["ok"], true, "{first}");
    assert_eq!(first["version"], 3);
    assert_eq!(first["run"]["strata"], 3, "{first}");
    assert_eq!(first["run"]["rules"], 3, "{first}");
    assert!(first.get("cache").is_none(), "{first}");
    assert_eq!(sorted_rows(&output.join("csvs/Mark.csv")), strings(&["2", "3", "4"]));
    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(output.join("csvs/stats.json")).unwrap()).unwrap();
    assert_eq!(written["rules"], 3, "{written}");

    fs::write(&program_path, ".this is not a FlowLog program\n").unwrap();
    let invalid = request(&socket, DaemonRequest::reload()).unwrap();
    let invalid: serde_json::Value = serde_json::from_str(&invalid).unwrap();
    assert_eq!(invalid["ok"], false);
    assert_eq!(invalid["diagnostic"]["kind"], "parse", "{invalid}");

    fs::write(&program_path, reach_program("Mark(x) :- Reach(2, x).")).unwrap();
    let edited = request(&socket, DaemonRequest::reload()).unwrap();
    let edited: serde_json::Value = serde_json::from_str(&edited).unwrap();
    assert_eq!(edited["ok"], true, "{edited}");
    assert_eq!(sorted_rows(&output.join("csvs/Mark.csv")), strings(&["3", "4"]));

    // Two programs over two fact directories, in flight at once, each with its
    // own output directory.
    let other_facts = temp.path("other-facts");
    fs::create_dir_all(&other_facts).unwrap();
    fs::write(other_facts.join("Edge.csv"), "5,6\n6,7\n").unwrap();
    let other_program = temp.path("other-program.dl");
    fs::write(&other_program, reach_program("Mark(x) :- Reach(5, x).")).unwrap();
    let other_output = temp.path("other-output");
    let concurrent = (0..2)
        .map(|index| {
            let socket = socket.clone();
            let (program, facts, csvs) = if index == 0 {
                (program_path.clone(), facts.clone(), output.clone())
            } else {
                (other_program.clone(), other_facts.clone(), other_output.clone())
            };
            std::thread::spawn(move || {
                request(
                    &socket,
                    DaemonRequest::Reload {
                        program: Some(program.to_str().unwrap().to_string()),
                        facts: Some(facts.to_str().unwrap().to_string()),
                        csvs: Some(csvs.to_str().unwrap().to_string()),
                    },
                )
                .unwrap()
            })
        })
        .collect::<Vec<_>>();
    let replies = concurrent
        .into_iter()
        .map(|handle| serde_json::from_str::<serde_json::Value>(&handle.join().unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(replies[0]["ok"], true, "{}", replies[0]);
    assert_eq!(replies[1]["ok"], true, "{}", replies[1]);
    assert_eq!(sorted_rows(&output.join("csvs/Mark.csv")), strings(&["3", "4"]));
    assert_eq!(sorted_rows(&other_output.join("csvs/Mark.csv")), strings(&["6", "7"]));

    let stats = request(&socket, DaemonRequest::Stats).unwrap();
    let stats: serde_json::Value = serde_json::from_str(&stats).unwrap();
    assert_eq!(stats["ok"], true);
    assert_eq!(stats["running"], 0, "{stats}");

    let shutdown = request(&socket, DaemonRequest::Shutdown).unwrap();
    let shutdown: serde_json::Value = serde_json::from_str(&shutdown).unwrap();
    assert_eq!(shutdown["ok"], true);
    daemon.join().unwrap().unwrap();
    assert!(!socket.exists(), "daemon left a stale socket behind");
}

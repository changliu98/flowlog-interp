//! End-to-end behaviour of the content-addressed state cache: what a rule edit
//! invalidates, what it does not, and that cached and clean runs agree.

use clap::Parser;
use executing::arg::Args;
use executing::cache::{DiskStore, StrataCache};
use executing::daemon::{request, serve, DaemonRequest};
use executing::runner::run_cached;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

struct TempTree(PathBuf);

impl TempTree {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should follow Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "flowlog-interp-cache-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create cache fixture directory");
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

fn program(mark_rule: &str) -> String {
    program_with(
        "Reach(x, y) :- Edge(x, y).",
        "Reach(x, z) :- Reach(x, y), Edge(y, z).",
        mark_rule,
        "",
    )
}

fn program_with(base: &str, step: &str, mark_rule: &str, extra_declarations: &str) -> String {
    format!(
        ".in\n\
         .decl Edge(x: number, y: number)\n\
         .input Edge.csv\n\
         .printsize\n\
         .decl Reach(x: number, y: number)\n\
         .decl Mark(x: number)\n\
         {extra_declarations}\
         .rule\n\
         {base}\n\
         {step}\n\
         {mark_rule}\n"
    )
}

fn args(program: &Path, facts: &Path, output: &Path) -> Args {
    args_with(program, facts, output, &[])
}

fn args_with(program: &Path, facts: &Path, output: &Path, extra: &[&str]) -> Args {
    let mut arguments = vec![
        "executing".to_string(),
        "--program".to_string(),
        program.to_str().unwrap().to_string(),
        "--facts".to_string(),
        facts.to_str().unwrap().to_string(),
        "--csvs".to_string(),
        output.to_str().unwrap().to_string(),
        "--workers".to_string(),
        "2".to_string(),
        "--cache-max-mib".to_string(),
        "64".to_string(),
    ];
    arguments.extend(extra.iter().map(|argument| (*argument).to_string()));
    Args::parse_from(arguments)
}

fn memory_cache() -> Mutex<StrataCache> {
    Mutex::new(StrataCache::new(64 * 1024 * 1024))
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

fn fixture(temp: &TempTree, mark_rule: &str) -> (PathBuf, PathBuf, PathBuf) {
    let facts = temp.path("facts");
    fs::create_dir_all(&facts).unwrap();
    fs::write(facts.join("Edge.csv"), "1,2\n2,3\n3,4\n").unwrap();
    let program_path = temp.path("program.dl");
    fs::write(&program_path, program(mark_rule)).unwrap();
    (program_path, facts, temp.path("cached-output"))
}

#[test]
fn a_downstream_rule_edit_reuses_the_recursive_upstream_stratum() {
    let temp = TempTree::new("downstream-edit");
    let (program_path, facts, output) = fixture(&temp, "Mark(x) :- Reach(1, x).");

    let cache = memory_cache();
    let cold = run_cached(args(&program_path, &facts, &output), &cache);
    assert_eq!((cold.hits, cold.misses), (0, 3), "{cold:?}");
    assert_eq!(cold.strata, 3);

    let warm = run_cached(args(&program_path, &facts, &output), &cache);
    assert_eq!((warm.hits, warm.misses), (3, 0));
    assert!(warm.rows_loaded > 0);

    fs::write(&program_path, program("Mark(x) :- Reach(2, x).")).unwrap();
    let edited = run_cached(args(&program_path, &facts, &output), &cache);
    assert_eq!((edited.hits, edited.misses), (2, 1));

    let clean_output = temp.path("clean-output");
    let clean_cache = memory_cache();
    let clean = run_cached(args(&program_path, &facts, &clean_output), &clean_cache);
    assert_eq!((clean.hits, clean.misses), (0, 3));

    for relation in ["Reach", "Mark"] {
        assert_eq!(
            sorted_rows(&output.join(format!("csvs/{relation}.csv"))),
            sorted_rows(&clean_output.join(format!("csvs/{relation}.csv"))),
            "cached and clean {relation} outputs differ"
        );
    }
    assert_eq!(
        sorted_rows(&output.join("csvs/Mark.csv")),
        vec!["3".to_string(), "4".to_string()]
    );

    fs::write(facts.join("Edge.csv"), "1,2\n2,3\n3,4\n4,5\n").unwrap();
    let changed_input = run_cached(args(&program_path, &facts, &output), &cache);
    assert_eq!((changed_input.hits, changed_input.misses), (0, 3));
}

#[test]
fn a_rule_rewritten_to_mean_the_same_thing_keeps_every_entry() {
    let temp = TempTree::new("respelled");
    let (program_path, facts, output) = fixture(&temp, "Mark(x) :- Reach(1, x).");
    let cache = memory_cache();
    let cold = run_cached(args(&program_path, &facts, &output), &cache);
    assert_eq!((cold.hits, cold.misses), (0, 3));

    // Renamed variables and a reordered body: the same rules.
    fs::write(
        &program_path,
        program_with(
            "Reach(a, b) :- Edge(a, b).",
            "Reach(p, r) :- Edge(q, r), Reach(p, q).",
            "Mark(m) :- Reach(1, m).",
            "",
        ),
    )
    .unwrap();
    let respelled = run_cached(args(&program_path, &facts, &output), &cache);
    assert_eq!((respelled.hits, respelled.misses), (3, 0), "{respelled:?}");

    // A declaration nothing derives: no unit touches it.
    fs::write(
        &program_path,
        program_with(
            "Reach(x, y) :- Edge(x, y).",
            "Reach(x, z) :- Reach(x, y), Edge(y, z).",
            "Mark(x) :- Reach(1, x).",
            ".decl Other(x: number)\n",
        ),
    )
    .unwrap();
    let declared = run_cached(args(&program_path, &facts, &output), &cache);
    assert_eq!((declared.hits, declared.misses), (3, 0), "{declared:?}");
}

#[test]
fn an_upstream_edit_that_leaves_the_rows_unchanged_stops_invalidating_there() {
    let temp = TempTree::new("cutoff");
    let (program_path, facts, output) = fixture(&temp, "Mark(x) :- Reach(1, x).");
    let cache = memory_cache();
    run_cached(args(&program_path, &facts, &output), &cache);

    // A second base rule that derives nothing new: the base unit's rules
    // changed, its rows did not, so the recursion and Mark are served.
    fs::write(
        &program_path,
        format!(
            "{}Reach(x, y) :- Edge(x, y), Edge(x, y).\n",
            program("Mark(x) :- Reach(1, x).")
        ),
    )
    .unwrap();
    let edited = run_cached(args(&program_path, &facts, &output), &cache);
    assert_eq!((edited.hits, edited.misses), (2, 1), "{edited:?}");
    assert_eq!(edited.cutoff_hits, 2, "{edited:?}");
    assert_eq!(
        sorted_rows(&output.join("csvs/Mark.csv")),
        vec!["2".to_string(), "3".to_string(), "4".to_string()]
    );

    // A base rule that does derive new rows invalidates everything below it.
    fs::write(
        &program_path,
        format!(
            "{}Reach(y, x) :- Edge(x, y).\n",
            program("Mark(x) :- Reach(1, x).")
        ),
    )
    .unwrap();
    let changed = run_cached(args(&program_path, &facts, &output), &cache);
    assert_eq!((changed.hits, changed.misses), (0, 3), "{changed:?}");
    assert!(sorted_rows(&output.join("csvs/Reach.csv")).contains(&"2,1".to_string()));
}

#[test]
fn the_disk_store_serves_a_process_that_never_computed() {
    let temp = TempTree::new("disk");
    let (program_path, facts, output) = fixture(&temp, "Mark(x) :- Reach(1, x).");
    let store = temp.path("state-store");

    let first = Mutex::new(
        StrataCache::new(64 * 1024 * 1024)
            .with_disk(DiskStore::new(store.clone(), 1 << 30)),
    );
    let cold = run_cached(args(&program_path, &facts, &output), &first);
    assert_eq!((cold.hits, cold.misses, cold.disk_hits), (0, 3, 0));

    let second = Mutex::new(
        StrataCache::new(64 * 1024 * 1024).with_disk(DiskStore::new(store, 1 << 30)),
    );
    let other_output = temp.path("other-output");
    let warm = run_cached(args(&program_path, &facts, &other_output), &second);
    assert_eq!((warm.hits, warm.misses, warm.disk_hits), (3, 0, 3), "{warm:?}");
    assert_eq!(
        sorted_rows(&output.join("csvs/Reach.csv")),
        sorted_rows(&other_output.join("csvs/Reach.csv"))
    );
    assert_eq!(
        sorted_rows(&other_output.join("csvs/Mark.csv")),
        vec!["2".to_string(), "3".to_string(), "4".to_string()]
    );

    // A one-shot process pointed at the store reads along it too.
    let one_shot_output = temp.path("one-shot-output");
    let one_shot = args_with(
        &program_path,
        &facts,
        &one_shot_output,
        &["--cache-dir", temp.path("state-store").to_str().unwrap()],
    );
    executing::runner::run_once(one_shot);
    let stats: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(one_shot_output.join("csvs/cache-stats.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(stats["hits"], 3);
    assert_eq!(stats["disk_hits"], 3);
    assert_eq!(stats["misses"], 0);
}

#[test]
fn the_daemon_serves_named_paths_concurrently_and_removes_its_socket() {
    let temp = TempTree::new("daemon");
    let (program_path, facts, output) = fixture(&temp, "Mark(x) :- Reach(1, x).");
    let socket = temp.path("flowlog.sock");

    let daemon_args = args(&program_path, &facts, &output);
    let daemon_socket = socket.clone();
    let daemon = std::thread::spawn(move || serve(daemon_args, daemon_socket));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !socket.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(socket.exists(), "daemon did not create its socket");

    let cold = request(&socket, DaemonRequest::reload()).unwrap();
    let cold: serde_json::Value = serde_json::from_str(&cold).unwrap();
    assert_eq!(cold["ok"], true, "{cold}");
    assert_eq!(cold["run"]["hits"], 0);
    assert_eq!(cold["run"]["misses"], 3);

    fs::write(&program_path, ".this is not a FlowLog program\n").unwrap();
    let invalid = request(&socket, DaemonRequest::reload()).unwrap();
    let invalid: serde_json::Value = serde_json::from_str(&invalid).unwrap();
    assert_eq!(invalid["ok"], false);
    assert_eq!(invalid["cache"]["entries"], 3);

    fs::write(&program_path, program("Mark(x) :- Reach(2, x).")).unwrap();
    let edited = request(&socket, DaemonRequest::reload()).unwrap();
    let edited: serde_json::Value = serde_json::from_str(&edited).unwrap();
    assert_eq!(edited["run"]["hits"], 2);
    assert_eq!(edited["run"]["misses"], 1);

    // Two programs over two fact directories, in flight at once, each with its
    // own output directory, sharing the cache.
    let other_facts = temp.path("other-facts");
    fs::create_dir_all(&other_facts).unwrap();
    fs::write(other_facts.join("Edge.csv"), "5,6\n6,7\n").unwrap();
    let other_program = temp.path("other-program.dl");
    fs::write(&other_program, program("Mark(x) :- Reach(5, x).")).unwrap();
    let other_output = temp.path("other-output");
    let concurrent = (0..2)
        .map(|index| {
            let socket = socket.clone();
            let program_path = program_path.clone();
            let facts = facts.clone();
            let output = output.clone();
            let other_program = other_program.clone();
            let other_facts = other_facts.clone();
            let other_output = other_output.clone();
            std::thread::spawn(move || {
                let (program, facts, csvs) = if index == 0 {
                    (program_path, facts, output)
                } else {
                    (other_program, other_facts, other_output)
                };
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
    assert_eq!(replies[0]["run"]["misses"], 0, "{}", replies[0]);
    assert_eq!(replies[1]["ok"], true, "{}", replies[1]);
    assert_eq!(replies[1]["run"]["misses"], 3, "{}", replies[1]);
    assert_eq!(
        sorted_rows(&other_output.join("csvs/Mark.csv")),
        vec!["6".to_string(), "7".to_string()]
    );

    let stats = request(&socket, DaemonRequest::Stats).unwrap();
    let stats: serde_json::Value = serde_json::from_str(&stats).unwrap();
    assert_eq!(stats["ok"], true);
    assert!(stats["cache"]["entries"].as_u64().unwrap() >= 6, "{stats}");

    let shutdown = request(&socket, DaemonRequest::Shutdown).unwrap();
    let shutdown: serde_json::Value = serde_json::from_str(&shutdown).unwrap();
    assert_eq!(shutdown["ok"], true);
    daemon.join().unwrap().unwrap();
    assert!(!socket.exists(), "daemon left a stale socket behind");
}

#[test]
fn cached_sideways_information_passing_keeps_private_boundaries() {
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
    let output = temp.path("output");
    let cache = memory_cache();

    let cold = run_cached(
        args_with(&program_path, &facts, &output, &["-O", "1"]),
        &cache,
    );
    assert!(cold.misses > 0);
    let warm = run_cached(
        args_with(&program_path, &facts, &output, &["-O", "1"]),
        &cache,
    );
    assert_eq!(warm.misses, 0);
    assert_eq!(warm.hits, cold.misses);
    assert_eq!(
        sorted_rows(&output.join("csvs/H.csv")),
        ["1,2,7", "1,4,7", "2,3,7"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    );
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
    // is derived from B), so the second unit reads H's earlier state.
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
    let cache = memory_cache();
    let cold = run_cached(args(&program_path, &facts, &output), &cache);
    assert_eq!(cold.misses, cold.units, "{cold:?}");
    assert_eq!(
        sorted_rows(&output.join("csvs/H.csv")),
        vec!["1".to_string(), "2".to_string(), "3".to_string()]
    );
    assert_eq!(
        sorted_rows(&output.join("csvs/K.csv")),
        vec!["1".to_string(), "2".to_string(), "3".to_string()]
    );
    let warm = run_cached(args(&program_path, &facts, &output), &cache);
    assert_eq!((warm.hits, warm.misses), (cold.units, 0), "{warm:?}");

    let clean_output = temp.path("clean-output");
    executing::runner::run_once(args(&program_path, &facts, &clean_output));
    for relation in ["G", "H", "K"] {
        assert_eq!(
            sorted_rows(&output.join(format!("csvs/{relation}.csv"))),
            sorted_rows(&clean_output.join(format!("csvs/{relation}.csv"))),
            "cached and one-shot {relation} outputs differ"
        );
    }
}

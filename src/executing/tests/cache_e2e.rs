//! End-to-end behaviour of the content-addressed state cache: what a rule edit
//! invalidates, what it does not, and that cached and clean runs agree.

use clap::Parser;
use executing::arg::Args;
use executing::cache::{DiskStore, StrataCache};
use executing::daemon::{request, serve, DaemonRequest};
use executing::runner::run_cached;
use std::collections::BTreeMap;
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

/// Compare every declared output with the uncached, whole-program dataflow,
/// rather than using another execution of the cache path as the oracle.
fn assert_clean_outputs(program: &Path, facts: &Path, output: &Path, extra: &[&str]) {
    let clean = output.with_extension("clean");
    executing::runner::run_once(args_with(program, facts, &clean, extra));
    let snapshot = |directory: &Path| -> BTreeMap<String, Vec<String>> {
        fs::read_dir(directory.join("csvs"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "csv"))
            .map(|path| {
                (
                    path.file_name().unwrap().to_str().unwrap().to_string(),
                    sorted_rows(&path),
                )
            })
            .collect()
    };
    assert_eq!(
        snapshot(output),
        snapshot(&clean),
        "cached output differs from clean evaluation"
    );
}

fn contribution_program(rules: &str) -> String {
    format!(
        ".in\n\
        .decl A(x: number)\n.input A.facts\n\
        .decl B(x: number)\n.input B.facts\n\
        .decl C(x: number)\n.input C.facts\n\
        .printsize\n.decl H(x: number)\n.rule\n{rules}\n"
    )
}

fn contribution_fixture(temp: &TempTree) -> (PathBuf, PathBuf) {
    let facts = temp.path("facts");
    fs::create_dir_all(&facts).unwrap();
    for (name, rows) in [("A", "1\n2\n"), ("B", "2\n3\n"), ("C", "3\n4\n")] {
        fs::write(facts.join(format!("{name}.facts")), rows).unwrap();
    }
    (temp.path("program.dl"), facts)
}

#[test]
fn clause_edits_reuse_contributions_and_deletions_preserve_other_support() {
    for extra in [vec![], vec!["--fat-mode"]] {
        let temp = TempTree::new("contribution-edits");
        let (program_path, facts) = contribution_fixture(&temp);
        let cache = memory_cache();
        // Start with two overlapping rules, then add, delete, replace, and
        // respell clauses. Later heads have not previously been cached whole.
        let edits = [
            ("H(x) :- A(x).\nH(x) :- B(x).", 0, 2, vec!["1", "2", "3"]),
            (
                "H(x) :- A(x).\nH(x) :- B(x).\nH(x) :- C(x).",
                2,
                1,
                vec!["1", "2", "3", "4"],
            ),
            ("H(x) :- B(x).\nH(x) :- C(x).", 2, 0, vec!["2", "3", "4"]),
            ("H(x) :- C(x).", 1, 0, vec!["3", "4"]),
            (
                "H(x) :- C(x).\nH(x) :- A(x), x > 1.",
                1,
                1,
                vec!["2", "3", "4"],
            ),
            (
                "H(a) :- C(a).\nH(b) :- b > 1, A(b).\nH(c) :- A(c), c > 1.",
                0,
                0,
                vec!["2", "3", "4"],
            ),
        ];
        for (index, (rules, hits, misses, expected)) in edits.into_iter().enumerate() {
            fs::write(&program_path, contribution_program(rules)).unwrap();
            let output = temp.path(&format!("edit-{index}"));
            let stats = run_cached(args_with(&program_path, &facts, &output, &extra), &cache);
            assert_eq!(
                (stats.contribution_hits, stats.contribution_misses),
                (hits, misses),
                "edit {index}: {stats:?}"
            );
            assert_eq!(stats.rules_evaluated, misses, "edit {index}: {stats:?}");
            if misses == 0 {
                assert_eq!(
                    stats.execution_micros, 0,
                    "a union of cached contributions started a dataflow"
                );
            }
            assert_eq!(sorted_rows(&output.join("csvs/H.csv")), expected);
            assert_clean_outputs(&program_path, &facts, &output, &extra);
        }
    }
}

#[test]
fn changed_positive_and_negative_inputs_invalidate_only_their_contributions() {
    let temp = TempTree::new("contribution-inputs");
    let (program_path, facts) = contribution_fixture(&temp);
    fs::write(
        &program_path,
        contribution_program("H(x) :- A(x).\nH(x) :- B(x), !C(x)."),
    )
    .unwrap();
    let cache = memory_cache();
    let output = temp.path("initial");
    run_cached(args(&program_path, &facts, &output), &cache);
    assert_clean_outputs(&program_path, &facts, &output, &[]);

    for (index, (name, rows, expected)) in [
        ("C", "4\n", vec!["1", "2", "3"]), // a negated tuple disappears
        ("A", "5\n", vec!["2", "3", "5"]), // positive support is replaced
        ("C", "2\n3\n4\n", vec!["5"]),     // all of B's contribution retracts
    ]
    .into_iter()
    .enumerate()
    {
        fs::write(facts.join(format!("{name}.facts")), rows).unwrap();
        let output = temp.path(&format!("input-{index}"));
        let stats = run_cached(args(&program_path, &facts, &output), &cache);
        assert_eq!(
            (
                stats.contribution_hits,
                stats.contribution_misses,
                stats.rules_evaluated
            ),
            (1, 1, 1),
            "{stats:?}"
        );
        assert_eq!(sorted_rows(&output.join("csvs/H.csv")), expected);
        assert_clean_outputs(&program_path, &facts, &output, &[]);
    }
}

#[test]
fn a_contribution_does_not_capture_inherited_head_rows() {
    let temp = TempTree::new("contribution-inherited");
    let (program_path, facts) = contribution_fixture(&temp);
    let source = contribution_program("H(x) :- A(x).\nG(x) :- B(x).\nH(x) :- G(x).").replace(
        ".decl H(x: number)",
        ".decl H(x: number)\n.decl G(x: number)",
    );
    fs::write(&program_path, &source).unwrap();
    let cache = memory_cache();
    run_cached(args(&program_path, &facts, &temp.path("initial")), &cache);

    // H's earlier state changes, while H <- G's body does not. If that
    // contribution captured the earlier H, the removed 1 would survive.
    fs::write(facts.join("A.facts"), "5\n").unwrap();
    let output = temp.path("replaced-input");
    let stats = run_cached(args(&program_path, &facts, &output), &cache);
    assert_eq!(
        (stats.contribution_hits, stats.contribution_misses),
        (1, 1),
        "{stats:?}"
    );
    assert_eq!(sorted_rows(&output.join("csvs/H.csv")), vec!["2", "3", "5"]);
    assert_clean_outputs(&program_path, &facts, &output, &[]);

    // Removing the earlier producer changes the layout, not the contribution.
    fs::write(&program_path, source.replace("H(x) :- A(x).", "")).unwrap();
    let output = temp.path("deleted-producer");
    let stats = run_cached(args(&program_path, &facts, &output), &cache);
    assert_eq!(stats.rules_evaluated, 0, "{stats:?}");
    assert_eq!(sorted_rows(&output.join("csvs/H.csv")), vec!["2", "3"]);
    assert_clean_outputs(&program_path, &facts, &output, &[]);
}

#[test]
fn recursive_support_and_aggregate_changes_use_whole_unit_evaluation() {
    let temp = TempTree::new("contribution-fallback");
    let (program_path, facts) = contribution_fixture(&temp);
    let source =
        contribution_program("H(x) :- A(x).\nQ(x) :- H(x).\nH(x) :- Q(x).\nM(min(x)) :- H(x).")
            .replace(
                ".decl H(x: number)",
                ".decl H(x: number)\n.decl Q(x: number)\n.decl M(x: number)",
            );
    fs::write(&program_path, &source).unwrap();
    let cache = memory_cache();
    let output = temp.path("initial");
    let initial = run_cached(args(&program_path, &facts, &output), &cache);
    assert_eq!(
        initial.contribution_misses, 1,
        "only the nonrecursive base clause is split: {initial:?}"
    );
    assert_clean_outputs(&program_path, &facts, &output, &[]);

    fs::write(facts.join("A.facts"), "2\n").unwrap();
    let output = temp.path("minimum-removed");
    run_cached(args(&program_path, &facts, &output), &cache);
    assert_eq!(sorted_rows(&output.join("csvs/M.csv")), vec!["2"]);
    assert_clean_outputs(&program_path, &facts, &output, &[]);

    fs::write(&program_path, source.replace("H(x) :- A(x).", "")).unwrap();
    let output = temp.path("support-removed");
    let removed = run_cached(args(&program_path, &facts, &output), &cache);
    assert_eq!(
        removed.contribution_hits + removed.contribution_misses,
        0,
        "{removed:?}"
    );
    assert_eq!(
        removed.rules_evaluated, 3,
        "recursive component and aggregate must re-evaluate: {removed:?}"
    );
    for name in ["H", "Q", "M"] {
        assert!(sorted_rows(&output.join(format!("csvs/{name}.csv"))).is_empty());
    }
    assert_clean_outputs(&program_path, &facts, &output, &[]);
}

#[test]
fn sideways_plans_keep_same_head_contributions_separate() {
    let temp = TempTree::new("contribution-sip");
    let facts = temp.path("facts");
    fs::create_dir_all(&facts).unwrap();
    for (name, rows) in [
        ("A", "1,2\n2,3\n"),
        ("B", "2,3\n3,4\n"),
        ("C", "3,5\n3,6\n4,6\n"),
    ] {
        fs::write(facts.join(format!("{name}.facts")), rows).unwrap();
    }
    let header = ".in\n\
        .decl A(x: number, y: number)\n.input A.facts\n\
        .decl B(x: number, y: number)\n.input B.facts\n\
        .decl C(x: number, y: number)\n.input C.facts\n\
        .printsize\n.decl H(x: number, z: number)\n.rule\n";
    let forward = "H(x, z) :- A(x, y), B(y, w), C(w, z).\n";
    let reverse = "H(z, x) :- A(x, y), B(y, w), C(w, z).\n";
    let program_path = temp.path("program.dl");
    let cache = memory_cache();
    let extra = ["-O", "3"];
    fs::write(&program_path, format!("{header}{forward}{reverse}")).unwrap();
    let initial_output = temp.path("initial");
    run_cached(
        args_with(&program_path, &facts, &initial_output, &extra),
        &cache,
    );
    assert_clean_outputs(&program_path, &facts, &initial_output, &extra);

    fs::write(&program_path, format!("{header}{reverse}")).unwrap();
    let output = temp.path("deleted");
    let stats = run_cached(args_with(&program_path, &facts, &output, &extra), &cache);
    assert_eq!(
        (stats.contribution_hits, stats.rules_evaluated),
        (1, 0),
        "{stats:?}"
    );
    assert_eq!(
        sorted_rows(&output.join("csvs/H.csv")),
        vec!["5,1", "6,1", "6,2"]
    );
    assert_clean_outputs(&program_path, &facts, &output, &extra);
}

#[test]
fn native_source_changes_invalidate_only_calling_contributions() {
    let temp = TempTree::new("contribution-native");
    let (program_path, facts) = contribution_fixture(&temp);
    let calls = temp.path("calls");
    let extra = ["--call-cache", calls.to_str().unwrap()];
    let cache = memory_cache();
    for increment in [1, 2] {
        let source = format!(
            ".code rust\npub fn shift(x: i64) -> i64 {{ x + {increment} }}\n.endcode\n{}",
            contribution_program("H(x) :- A(x).\nH(y) :- B(x), y = @call(shift, x).")
        );
        fs::write(&program_path, source).unwrap();
        let output = temp.path(&format!("native-{increment}"));
        let stats = run_cached(args_with(&program_path, &facts, &output, &extra), &cache);
        if increment == 2 {
            assert_eq!(
                (stats.contribution_hits, stats.contribution_misses),
                (1, 1),
                "{stats:?}"
            );
            assert_eq!(
                sorted_rows(&output.join("csvs/H.csv")),
                vec!["1", "2", "4", "5"]
            );
        }
        assert_clean_outputs(&program_path, &facts, &output, &extra);
    }
}

#[test]
fn one_shot_processes_share_contributions_for_a_new_rule_revision() {
    let temp = TempTree::new("contribution-processes");
    let (program_path, facts) = contribution_fixture(&temp);
    let store = temp.path("store");
    for (index, rules) in ["H(x) :- A(x).\nH(x) :- B(x).", "H(x) :- B(x)."]
        .into_iter()
        .enumerate()
    {
        fs::write(&program_path, contribution_program(rules)).unwrap();
        let output = temp.path(&format!("process-{index}"));
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_executing"))
            .arg("--program")
            .arg(&program_path)
            .arg("--facts")
            .arg(&facts)
            .arg("--csvs")
            .arg(&output)
            .arg("--cache-dir")
            .arg(&store)
            .args(["--workers", "2"])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let stats: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("csvs/cache-stats.json")).unwrap())
                .unwrap();
        if index == 1 {
            assert_eq!(stats["misses"], 1);
            assert_eq!(stats["contribution_disk_hits"], 1);
            assert_eq!(stats["contribution_misses"], 0);
            assert_eq!(stats["rules_evaluated"], 0);
            assert_eq!(sorted_rows(&output.join("csvs/H.csv")), vec!["2", "3"]);
        }
        assert_clean_outputs(&program_path, &facts, &output, &[]);
    }
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
        StrataCache::new(64 * 1024 * 1024).with_disk(DiskStore::new(store.clone(), 1 << 30)),
    );
    let cold = run_cached(args(&program_path, &facts, &output), &first);
    assert_eq!((cold.hits, cold.misses, cold.disk_hits), (0, 3, 0));

    let second =
        Mutex::new(StrataCache::new(64 * 1024 * 1024).with_disk(DiskStore::new(store, 1 << 30)));
    let other_output = temp.path("other-output");
    let warm = run_cached(args(&program_path, &facts, &other_output), &second);
    assert_eq!(
        (warm.hits, warm.misses, warm.disk_hits),
        (3, 0, 3),
        "{warm:?}"
    );
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
    assert_eq!(invalid["cache"]["entries"], cold["cache"]["entries"]);

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

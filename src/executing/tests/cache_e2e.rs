use clap::Parser;
use executing::arg::Args;
use executing::cache::StrataCache;
use executing::runner::run_cached;
use std::fs;
use std::path::{Path, PathBuf};
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
    ];
    arguments.extend(extra.iter().map(|argument| (*argument).to_string()));
    Args::parse_from(arguments)
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

#[test]
fn a_downstream_rule_edit_reuses_the_recursive_upstream_stratum() {
    let temp = TempTree::new("downstream-edit");
    let facts = temp.path("facts");
    fs::create_dir_all(&facts).unwrap();
    fs::write(facts.join("Edge.csv"), "1,2\n2,3\n3,4\n").unwrap();
    let program_path = temp.path("program.dl");
    let output = temp.path("cached-output");
    fs::write(&program_path, program("Mark(x) :- Reach(1, x).")).unwrap();

    let mut cache = StrataCache::new(64 * 1024 * 1024);
    let cold = run_cached(args(&program_path, &facts, &output), &mut cache);
    assert_eq!((cold.hits, cold.misses), (0, 3));

    let warm = run_cached(args(&program_path, &facts, &output), &mut cache);
    assert_eq!((warm.hits, warm.misses), (3, 0));
    assert!(warm.rows_loaded > 0);

    fs::write(&program_path, program("Mark(x) :- Reach(2, x).")).unwrap();
    let edited = run_cached(args(&program_path, &facts, &output), &mut cache);
    assert_eq!((edited.hits, edited.misses), (2, 1));

    let clean_output = temp.path("clean-output");
    let mut clean_cache = StrataCache::new(64 * 1024 * 1024);
    let clean = run_cached(args(&program_path, &facts, &clean_output), &mut clean_cache);
    assert_eq!((clean.hits, clean.misses), (0, 3));

    for relation in ["Reach", "Mark"] {
        assert_eq!(
            sorted_rows(&output.join(format!("csvs/{relation}.csv"))),
            sorted_rows(&clean_output.join(format!("csvs/{relation}.csv"))),
            "cached and clean {relation} outputs differ"
        );
    }

    fs::write(facts.join("Edge.csv"), "1,2\n2,3\n3,4\n4,5\n").unwrap();
    let changed_input = run_cached(args(&program_path, &facts, &output), &mut cache);
    assert_eq!((changed_input.hits, changed_input.misses), (0, 3));
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
    let mut cache = StrataCache::new(64 * 1024 * 1024);

    let cold = run_cached(
        args_with(&program_path, &facts, &output, &["-O", "1"]),
        &mut cache,
    );
    assert!(cold.misses > 0);
    let warm = run_cached(
        args_with(&program_path, &facts, &output, &["-O", "1"]),
        &mut cache,
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

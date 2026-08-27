use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempTree(PathBuf);

impl TempTree {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("flowlog-call-e2e-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn embedded_rust_calls_bind_filter_chain_and_reuse_the_native_cache() {
    let temp = TempTree::new();
    let program_path = temp.0.join("calls.dl");
    let facts_path = temp.0.join("facts");
    let output_path = temp.0.join("output");
    let cache_path = temp.0.join("cache");
    fs::create_dir_all(&facts_path).unwrap();

    fs::write(
        &program_path,
        r#".code rust
use std::cmp::min;

fn absolute(value: i32) -> i32 {
    value.saturating_abs()
}

pub fn normalize(value: i32, limit: i32) -> i32 {
    min(absolute(value), limit)
}

pub fn add(value: i32, increment: i32) -> i32 {
    value.saturating_add(increment)
}

pub fn keep(value: i32) -> bool {
    value % 2 == 0
}
.endcode
.in
.decl Input(id: number, value: number)
.input Input.facts
.decl Limit(id: number, value: number)
.input Limit.facts
.decl Bias(id: number, value: number)
.input Bias.facts
.printsize
.decl Result(id: number, original: number, normalized: number, adjusted: number)
.rule
Result(ID, X, Y, Z) :- Input(ID, X), Limit(ID, L), Bias(ID, B), Y = @call(normalize, X, L), Z = @call(add, Y, B), @call(keep, Z).
"#,
    )
    .unwrap();
    fs::write(facts_path.join("Input.facts"), "1,-3\n2,2\n3,300\n4,-4\n").unwrap();
    fs::write(
        facts_path.join("Limit.facts"),
        "1,255\n2,255\n3,255\n4,255\n",
    )
    .unwrap();
    fs::write(facts_path.join("Bias.facts"), "1,1\n2,1\n3,1\n4,1\n").unwrap();

    let first = run_flowlog(
        &program_path,
        &facts_path,
        &output_path,
        &cache_path,
        2,
        false,
        false,
    );
    assert_success(&first);

    let mut rows = fs::read_to_string(output_path.join("csvs/Result.csv"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    rows.sort();
    assert_eq!(rows, vec!["1, -3, 3, 4", "3, 300, 255, 256"]);

    let library = find_library(&cache_path).expect("compiled call library missing");
    let first_modified = fs::metadata(&library).unwrap().modified().unwrap();

    let second = run_flowlog(
        &program_path,
        &facts_path,
        &output_path,
        &cache_path,
        1,
        true,
        true,
    );
    assert_success(&second);
    let mut fat_rows = fs::read_to_string(output_path.join("csvs/Result.csv"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    fat_rows.sort();
    assert_eq!(fat_rows, vec!["1, -3, 3, 4", "3, 300, 255, 256"]);
    let second_modified = fs::metadata(&library).unwrap().modified().unwrap();
    assert_eq!(
        first_modified, second_modified,
        "a cache hit unexpectedly recompiled the native module"
    );
}

#[test]
fn embedded_rust_call_participates_in_a_recursive_fixed_point() {
    let temp = TempTree::new();
    let program_path = temp.0.join("recursive-calls.dl");
    let facts_path = temp.0.join("facts");
    let output_path = temp.0.join("output");
    let cache_path = temp.0.join("cache");
    fs::create_dir_all(&facts_path).unwrap();

    fs::write(
        &program_path,
        r#".code rust
pub fn next(value: i32) -> i32 {
    value.saturating_add(1)
}

pub fn at_most(value: i32, limit: i32) -> bool {
    value <= limit
}
.endcode
.in
.decl Seed(value: number)
.input Seed.facts
.printsize
.decl Reach(value: number)
.rule
Reach(X) :- Seed(X).
Reach(Y) :- Reach(X), Y = @call(next, X), @call(at_most, Y, 3).
"#,
    )
    .unwrap();
    fs::write(facts_path.join("Seed.facts"), "0\n").unwrap();

    let execution = run_flowlog(
        &program_path,
        &facts_path,
        &output_path,
        &cache_path,
        2,
        false,
        false,
    );
    assert_success(&execution);

    let mut rows = fs::read_to_string(output_path.join("csvs/Reach.csv"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    rows.sort();
    assert_eq!(rows, vec!["0", "1", "2", "3"]);
}

fn run_flowlog(
    program: &Path,
    facts: &Path,
    output: &Path,
    cache: &Path,
    workers: usize,
    fat_mode: bool,
    optimize: bool,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_executing"));
    command
        .arg("--program")
        .arg(program)
        .arg("--facts")
        .arg(facts)
        .arg("--csvs")
        .arg(output)
        .arg("--call-cache")
        .arg(cache)
        .arg("--workers")
        .arg(workers.to_string());
    if fat_mode {
        command.arg("--fat-mode");
    }
    if optimize {
        command.arg("-O").arg("3");
    }
    command.output().unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "FlowLog failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn find_library(root: &Path) -> Option<PathBuf> {
    for entry in fs::read_dir(root).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find_library(&path) {
                return Some(found);
            }
        } else if path.extension().and_then(|value| value.to_str())
            == Some(env::consts::DLL_EXTENSION)
        {
            return Some(path);
        }
    }
    None
}

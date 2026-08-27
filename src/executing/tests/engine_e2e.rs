//! End-to-end regressions for engine behaviour that is only observable in a
//! complete run: the aggregation kernels, the input reader, and the shapes the
//! engine refuses.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempTree(PathBuf);

impl TempTree {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "flowlog-engine-e2e-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(path.join("facts")).unwrap();
        Self(path)
    }

    fn program(&self, source: &str) -> PathBuf {
        let path = self.0.join("program.dl");
        fs::write(&path, source).unwrap();
        path
    }

    fn facts(&self, relation: &str, rows: &str) -> PathBuf {
        let path = self.0.join("facts").join(format!("{relation}.facts"));
        fs::write(&path, rows).unwrap();
        self.0.join("facts")
    }

    fn output(&self, relation: &str) -> PathBuf {
        self.0.join("output/csvs").join(format!("{relation}.csv"))
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(temp: &TempTree, program: &Path, extra: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_executing"));
    command
        .arg("--program")
        .arg(program)
        .arg("--facts")
        .arg(temp.0.join("facts"))
        .arg("--csvs")
        .arg(temp.0.join("output"));
    for argument in extra {
        command.arg(argument);
    }
    command.output().unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "FlowLog failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn assert_refused(output: &Output, expected: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "FlowLog accepted a program it should refuse\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(
        stderr.contains(expected),
        "the refusal did not mention {expected:?}\nstderr:\n{stderr}",
    );
}

/// Reads an output relation as numbers, so that a test asserting *values* does
/// not also assert the file's formatting.
fn rows(path: &Path) -> Vec<Vec<i64>> {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read output {}: {error}", path.display()));
    let mut rows = text
        .lines()
        .map(|line| {
            line.split(',')
                .map(|cell| {
                    cell.trim()
                        .parse::<i64>()
                        .unwrap_or_else(|error| panic!("output cell {cell:?}: {error}"))
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    rows.sort();
    rows
}

#[test]
fn a_cell_that_is_not_a_number_refuses_the_run() {
    let temp = TempTree::new("bad-cell");
    let program = temp.program(
        ".in
.decl E(k: number, v: number)
.input E.facts
.printsize
.decl R(k: number, v: number)
.rule
R(k, v) :- E(k, v).
",
    );
    temp.facts("E", "1,2\n3,abc\n4,5\n");

    let execution = run(&temp, &program, &[]);
    assert_refused(&execution, "cell \"abc\" is not a number");
    assert_refused(&execution, "E.facts");
    assert_refused(&execution, "on line \"3,abc\"");
}

#[test]
fn a_cell_outside_the_value_domain_refuses_the_run() {
    let temp = TempTree::new("overflow-cell");
    let program = temp.program(
        ".in
.decl E(k: number, v: number)
.input E.facts
.printsize
.decl R(k: number, v: number)
.rule
R(k, v) :- E(k, v).
",
    );
    temp.facts("E", "1,9223372036854775808\n");

    assert_refused(&run(&temp, &program, &[]), "is not a number");
}

#[test]
fn the_whole_value_domain_survives_a_round_of_evaluation() {
    let temp = TempTree::new("domain");
    let program = temp.program(
        ".in
.decl E(k: number, v: number)
.input E.facts
.printsize
.decl R(k: number, v: number)
.rule
R(k, v) :- E(k, v), v > 4294967295.
",
    );
    temp.facts(
        "E",
        "1,9223372036854775807\n2,-9223372036854775808\n3,4294967296\n4,7\n",
    );

    assert_success(&run(&temp, &program, &[]));
    assert_eq!(
        rows(&temp.output("R")),
        vec![vec![1, 9223372036854775807], vec![3, 4294967296]],
    );
}

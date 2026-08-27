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

const MIN_PROGRAM: &str = ".in
.decl E(k: number, v: number)
.input E.facts
.printsize
.decl M(k: number, v: number)
.rule
M(k, min(v)) :- E(k, v).
";

#[test]
fn min_aggregation_orders_negative_values_below_positive_ones() {
    let temp = TempTree::new("min");
    let program = temp.program(MIN_PROGRAM);
    temp.facts("E", "1,3\n1,-4\n1,7\n2,-1\n2,-9\n3,5\n");

    assert_success(&run(&temp, &program, &[]));
    assert_eq!(
        rows(&temp.output("M")),
        vec![vec![1, -4], vec![2, -9], vec![3, 5]],
    );
}

#[test]
fn min_aggregation_orders_negative_values_below_positive_ones_in_fat_mode() {
    let temp = TempTree::new("min-fat");
    let program = temp.program(MIN_PROGRAM);
    temp.facts("E", "1,3\n1,-4\n1,7\n2,-1\n2,-9\n3,5\n");

    assert_success(&run(&temp, &program, &["--fat-mode"]));
    assert_eq!(
        rows(&temp.output("M")),
        vec![vec![1, -4], vec![2, -9], vec![3, 5]],
    );
}

/// One relation wider than the fixed-size row representation switches the whole
/// program to fat rows, so an unrelated aggregate in the same program is
/// evaluated by the fat kernel. That kernel used to emit nothing at all.
#[test]
fn an_aggregate_beside_a_wide_relation_still_produces_rows() {
    let temp = TempTree::new("fat-aggregate");
    let program = temp.program(
        ".in
.decl W(a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number, i: number)
.input W.facts
.decl E(k: number, v: number)
.input E.facts
.printsize
.decl S(k: number, v: number)
.decl C(k: number, v: number)
.decl X(k: number, v: number)
.rule
S(k, sum(v)) :- E(k, v).
C(k, count(v)) :- E(k, v).
X(a, i) :- W(a, b, c, d, e, f, g, h, i).
",
    );
    temp.facts("W", "1,2,3,4,5,6,7,8,9\n");
    temp.facts("E", "1,3\n1,4\n2,10\n2,-4\n");

    let execution = run(&temp, &program, &[]);
    assert_success(&execution);
    assert!(
        String::from_utf8_lossy(&execution.stdout).contains("Fat mode automatically enabled"),
        "the wide relation should have switched the program to fat rows",
    );

    assert_eq!(rows(&temp.output("S")), vec![vec![1, 7], vec![2, 6]]);
    assert_eq!(rows(&temp.output("C")), vec![vec![1, 2], vec![2, 2]]);
    assert_eq!(rows(&temp.output("X")), vec![vec![1, 9]]);
}

/// A relation the engine wrote must be a relation the engine can read: the
/// output of one run is the input of the next, and it is how a pipeline of
/// programs is composed at all.
#[test]
fn a_written_relation_reads_back_as_an_input_relation() {
    const IDENTITY: &str = ".in
.decl E(k: number, v: number)
.input E.facts
.printsize
.decl R(k: number, v: number)
.rule
R(k, v) :- E(k, v).
";

    let first = TempTree::new("roundtrip-write");
    let program = first.program(IDENTITY);
    first.facts("E", "1,2\n3,-4\n9223372036854775807,0\n");
    assert_success(&run(&first, &program, &[]));

    let written = fs::read_to_string(first.output("R")).unwrap();
    assert_eq!(
        {
            let mut lines = written.lines().collect::<Vec<_>>();
            lines.sort();
            lines
        },
        vec!["1,2", "3,-4", "9223372036854775807,0"],
    );

    let second = TempTree::new("roundtrip-read");
    let program = second.program(IDENTITY);
    second.facts("E", &written);
    assert_success(&run(&second, &program, &[]));

    assert_eq!(rows(&second.output("R")), rows(&first.output("R")));
}

#[test]
fn a_written_relation_uses_the_configured_delimiter() {
    let temp = TempTree::new("roundtrip-tab");
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
    temp.facts("E", "1\t2\n3\t-4\n");

    assert_success(&run(&temp, &program, &["--delimiter", "\t"]));
    assert_eq!(
        {
            let mut lines = fs::read_to_string(temp.output("R"))
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>();
            lines.sort();
            lines
        },
        vec!["1\t2", "3\t-4"],
    );
}

/// A cross-join - two atoms sharing no variable - is ordinary Datalog, and the
/// generated fixed-size product table reaches only `PROD_MAX = 2`. Both of
/// these shapes are outside it: joining a singleton against a graph relation is
/// the common case, and neither operand nor output is wide by any other
/// measure.
#[test]
fn a_cross_join_wider_than_the_generated_product_table_still_runs() {
    let temp = TempTree::new("cross-2-1-3");
    let program = temp.program(
        ".in
.decl Round(r: number)
.input Round.facts
.decl Arc(x: number, y: number)
.input Arc.facts
.printsize
.decl Out(r: number, x: number, y: number)
.rule
Out(r, x, y) :- Round(r), Arc(x, y).
",
    );
    temp.facts("Round", "7\n8\n");
    temp.facts("Arc", "1,2\n3,4\n");

    assert_success(&run(&temp, &program, &[]));
    assert_eq!(
        rows(&temp.output("Out")),
        vec![
            vec![7, 1, 2],
            vec![7, 3, 4],
            vec![8, 1, 2],
            vec![8, 3, 4],
        ],
    );
}

#[test]
fn a_cross_join_of_a_singleton_against_a_wide_relation_still_runs() {
    let temp = TempTree::new("cross-1-3-4");
    let program = temp.program(
        ".in
.decl Round(r: number)
.input Round.facts
.decl Tri(x: number, y: number, z: number)
.input Tri.facts
.printsize
.decl Out(r: number, x: number, y: number, z: number)
.rule
Out(r, x, y, z) :- Round(r), Tri(x, y, z).
",
    );
    temp.facts("Round", "7\n8\n");
    temp.facts("Tri", "1,2,3\n4,5,6\n");

    let expected = vec![
        vec![7, 1, 2, 3],
        vec![7, 4, 5, 6],
        vec![8, 1, 2, 3],
        vec![8, 4, 5, 6],
    ];

    assert_success(&run(&temp, &program, &[]));
    assert_eq!(rows(&temp.output("Out")), expected);

    // The same product on the globally fat representation, which is the
    // implementation the fixed-size fallback routes through.
    let fat = TempTree::new("cross-1-3-4-fat");
    let fat_program = fat.program(&fs::read_to_string(&program).unwrap());
    fat.facts("Round", "7\n8\n");
    fat.facts("Tri", "1,2,3\n4,5,6\n");
    assert_success(&run(&fat, &fat_program, &["--fat-mode"]));
    assert_eq!(rows(&fat.output("Out")), expected);
}

/// The head checks are unit-tested in `parsing::validate`; this asserts that
/// the binary reaches them, before it reads a fact or assembles a dataflow.
#[test]
fn a_head_the_engine_cannot_evaluate_refuses_the_run() {
    let temp = TempTree::new("head-constant");
    let program = temp.program(
        ".in
.decl E(k: number, v: number)
.input E.facts
.printsize
.decl R(k: number, v: number)
.rule
R(k, 7) :- E(k, v).
",
    );
    temp.facts("E", "1,2\n3,4\n");

    assert_refused(
        &run(&temp, &program, &[]),
        "head constants and head arithmetic are not supported",
    );
}

#[test]
fn a_string_column_refuses_the_run() {
    let temp = TempTree::new("string-column");
    let program = temp.program(
        ".in
.decl E(k: number, v: string)
.input E.facts
.printsize
.decl R(k: number)
.rule
R(k) :- E(k, v).
",
    );
    temp.facts("E", "1,hello\n2,world\n");

    assert_refused(
        &run(&temp, &program, &[]),
        "string columns are not implemented",
    );
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

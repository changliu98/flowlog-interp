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

/// An aggregate's group-by key is an ordinary fixed-size row arranged by
/// `reduce_core`, not one half of a generated join table, so every relation the
/// rows can hold should have an arm. The table stopped at `KV_MAX + 1 = 5`.
#[test]
fn an_aggregate_wider_than_the_key_value_tables_still_runs() {
    for (operator, expected) in [("sum", 13), ("min", 6), ("max", 7), ("count", 2)] {
        let temp = TempTree::new(&format!("wide-aggregate-{operator}"));
        let program = temp.program(&format!(
            ".in
.decl E(a: number, b: number, c: number, d: number, e: number, v: number)
.input E.facts
.printsize
.decl R(a: number, b: number, c: number, d: number, e: number, s: number)
.rule
R(a, b, c, d, e, {operator}(v)) :- E(a, b, c, d, e, v).
"
        ));
        temp.facts("E", "1,2,3,4,5,6\n1,2,3,4,5,7\n");

        assert_success(&run(&temp, &program, &[]));
        assert_eq!(
            rows(&temp.output("R")),
            vec![vec![1, 2, 3, 4, 5, expected]],
            "aggregate {operator}",
        );
    }
}

/// A join whose value side is wider than the key/value tables is ordinary as
/// long as its relations fit in a row. It has no fixed-size arm, so the program
/// must be planned onto fat rows instead of reaching one.
#[test]
fn a_join_with_a_value_side_wider_than_the_key_value_tables_still_runs() {
    let temp = TempTree::new("wide-kv-join");
    let program = temp.program(
        ".in
.decl A(k: number, a1: number, a2: number, a3: number, a4: number, a5: number)
.input A.facts
.decl B(k: number, b1: number)
.input B.facts
.printsize
.decl Out(k: number, a1: number, a2: number, a3: number, a4: number, a5: number, b1: number)
.rule
Out(k, a1, a2, a3, a4, a5, b1) :- A(k, a1, a2, a3, a4, a5), B(k, b1).
",
    );
    temp.facts("A", "1,2,3,4,5,6\n2,0,0,0,0,0\n");
    temp.facts("B", "1,9\n3,9\n");

    let execution = run(&temp, &program, &[]);
    assert_success(&execution);
    assert!(
        String::from_utf8_lossy(&execution.stdout).contains("Fat mode automatically enabled"),
        "a (1, 5) key/value split has no fixed-size arm and must be planned onto fat rows",
    );
    assert_eq!(
        rows(&temp.output("Out")),
        vec![vec![1, 2, 3, 4, 5, 6, 9]],
    );
}

/// An antijoin keeps its left row, and that row is bounded by the row limit
/// rather than by the key/value tables the antijoin is built from.
#[test]
fn an_antijoin_keeping_more_columns_than_the_key_value_tables_still_runs() {
    let temp = TempTree::new("wide-antijoin");
    let program = temp.program(
        ".in
.decl A(k: number, a1: number, a2: number, a3: number, a4: number)
.input A.facts
.decl B(k: number)
.input B.facts
.printsize
.decl Out(k: number, a1: number, a2: number, a3: number, a4: number)
.rule
Out(k, a1, a2, a3, a4) :- A(k, a1, a2, a3, a4), !B(k).
",
    );
    temp.facts("A", "1,2,3,4,5\n9,8,7,6,5\n");
    temp.facts("B", "9\n");

    assert_success(&run(&temp, &program, &[]));
    assert_eq!(rows(&temp.output("Out")), vec![vec![1, 2, 3, 4, 5]]);
}

/// An atom none of whose columns are retained downstream is an existential
/// guard: it contributes that the relation has a row, and nothing else.
/// "derive if any row exists" is ordinary Datalog and the planner used to have
/// no signature for it, refusing the rule with `kv_to_kv: null signatures`.
///
/// The trigger is the retained signature, not the spelling, so every way of
/// writing a dead column is covered: all positions wildcards, a named variable
/// nothing downstream reads, and both again in a rule carrying an embedded
/// call (which takes a different planning path, since calls skip SIP).
const GUARD_SPELLINGS: [(&str, &str, &str); 4] = [
    ("wildcards", "", "out(A) :- s(A), t(_, _)."),
    ("dead variable", "", "out(A) :- s(A), t(B, _)."),
    (
        "wildcards with a call",
        ".code rust\npub fn pick(x: i64) -> i64 { x }\n.endcode\n",
        "out(Y) :- s(A), t(_, _), Y = @call(pick, A).",
    ),
    (
        "dead variable with a call",
        ".code rust\npub fn pick(x: i64) -> i64 { x }\n.endcode\n",
        "out(Y) :- s(A), t(B, _), Y = @call(pick, A).",
    ),
];

fn guard_program(embedded: &str, rule: &str) -> String {
    format!(
        "{embedded}.in
.decl s(c0: number)
.input s.facts
.decl t(c0: number, c1: number)
.input t.facts
.printsize
.decl out(c0: number)
.rule
{rule}
"
    )
}

#[test]
fn an_existential_guard_contributes_existence() {
    for (spelling, embedded, rule) in GUARD_SPELLINGS {
        let temp = TempTree::new("guard");
        let program = temp.program(&guard_program(embedded, rule));
        temp.facts("s", "5\n");
        temp.facts("t", "7,8\n");

        assert_success(&run(&temp, &program, &[]));
        assert_eq!(rows(&temp.output("out")), vec![vec![5]], "{spelling}");
    }
}

#[test]
fn an_existential_guard_over_an_empty_relation_derives_nothing() {
    for (spelling, embedded, rule) in GUARD_SPELLINGS {
        let temp = TempTree::new("guard-empty");
        let program = temp.program(&guard_program(embedded, rule));
        temp.facts("s", "5\n");
        temp.facts("t", "");

        assert_success(&run(&temp, &program, &[]));
        assert_eq!(rows(&temp.output("out")), Vec::<Vec<i64>>::new(), "{spelling}");
    }
}

/// The guard says whether the relation has a row, so its own cardinality must
/// not reach the result: three rows of `t` are one existence, not three.
#[test]
fn an_existential_guard_does_not_multiply_its_driver() {
    for (spelling, embedded, rule) in GUARD_SPELLINGS {
        let temp = TempTree::new("guard-cardinality");
        let program = temp.program(&guard_program(embedded, rule));
        temp.facts("s", "5\n6\n");
        temp.facts("t", "7,8\n9,10\n11,12\n");

        assert_success(&run(&temp, &program, &[]));
        assert_eq!(
            rows(&temp.output("out")),
            vec![vec![5], vec![6]],
            "{spelling}",
        );
    }
}

#[test]
fn an_existential_guard_in_a_recursive_rule_reaches_its_fixed_point() {
    for (spelling, guard) in [("wildcards", "t(_, _)"), ("dead variable", "t(B, _)")] {
        let temp = TempTree::new("guard-recursive");
        let program = temp.program(&format!(
            ".in
.decl s(c0: number)
.input s.facts
.decl arc(x: number, y: number)
.input arc.facts
.decl t(c0: number, c1: number)
.input t.facts
.printsize
.decl reach(c0: number)
.rule
reach(A) :- s(A), {guard}.
reach(y) :- reach(x), arc(x, y).
"
        ));
        temp.facts("s", "5\n");
        temp.facts("arc", "5,6\n6,7\n");
        temp.facts("t", "7,8\n");

        assert_success(&run(&temp, &program, &[]));
        assert_eq!(
            rows(&temp.output("reach")),
            vec![vec![5], vec![6], vec![7]],
            "{spelling}",
        );
    }
}

/// The negated form of the same shape is a different question - whether the
/// relation is empty at all - and an antijoin has no key to answer it on. It is
/// refused rather than planned onto an absent key.
#[test]
fn a_negation_retaining_no_column_is_refused() {
    let temp = TempTree::new("guard-negated");
    let program = temp.program(&guard_program("", "out(A) :- s(A), !t(_, _)."));
    temp.facts("s", "5\n");
    temp.facts("t", "7,8\n");

    assert_refused(
        &run(&temp, &program, &[]),
        "negates t without retaining any of its columns",
    );
}

/// An antijoin keeps the body solutions no negated row matches, and only then
/// projects them onto the head. The two steps do not commute: `L` below is read
/// by the negated atom and dropped by the head, so two solutions that differ
/// only in `L` project onto one row.
///
/// Subtracting the *projected* rows answers a different question, and answered
/// it differently in each build - the default build derived a row every one of
/// whose solutions was blocked, and the isize build dropped a row that had an
/// unblocked solution. Both are silently wrong answers.
const ANTIJOIN_PROGRAM: &str = ".in
.decl candidate(f: number, o: number, l: number)
.input candidate.facts
.decl blocked(f: number, o: number, l: number)
.input blocked.facts
.printsize
.decl unblocked(f: number, o: number)
.rule
unblocked(F, O) :- candidate(F, O, L), !blocked(F, O, L).
";

fn antijoin_case(label: &str, candidates: &str, blocked: &str, expected: Vec<Vec<i64>>) {
    let temp = TempTree::new(&format!("antijoin-{label}"));
    let program = temp.program(ANTIJOIN_PROGRAM);
    temp.facts("candidate", candidates);
    temp.facts("blocked", blocked);

    assert_success(&run(&temp, &program, &[]));
    assert_eq!(rows(&temp.output("unblocked")), expected, "{label}");
}

#[test]
fn an_antijoin_answers_per_solution_and_not_per_projected_row() {
    // One solution: the case that was already right.
    antijoin_case("one-blocked", "0,2,3\n", "0,2,3\n", vec![]);
    antijoin_case("one-unblocked", "0,2,3\n", "", vec![vec![0, 2]]);

    // Two solutions projecting onto one head row: the bug. Every solution
    // blocked means the row is not derived.
    antijoin_case("two-all-blocked", "0,2,3\n0,2,4\n", "0,2,3\n0,2,4\n", vec![]);
    antijoin_case(
        "three-all-blocked",
        "0,2,3\n0,2,4\n0,2,5\n",
        "0,2,3\n0,2,4\n0,2,5\n",
        vec![],
    );

    // ...and one surviving solution still derives it, which is the answer the
    // set-difference-after-projection reading loses.
    antijoin_case("two-one-blocked", "0,2,3\n0,2,4\n", "0,2,3\n", vec![vec![0, 2]]);
    antijoin_case("two-none-blocked", "0,2,3\n0,2,4\n", "", vec![vec![0, 2]]);

    // Two head rows, each decided on its own solutions.
    antijoin_case(
        "distinct-heads",
        "0,2,3\n0,2,4\n0,5,3\n0,5,4\n",
        "0,2,3\n0,2,4\n0,5,3\n",
        vec![vec![0, 5]],
    );
}

/// The same shape where the antijoin's positive side carries no value beyond
/// its key, which is a different operator (`NjKK`) and had the same defect.
#[test]
fn a_key_only_antijoin_answers_per_solution() {
    for (label, blocked, expected) in [
        ("all blocked", "0,3\n0,4\n", Vec::new()),
        ("one blocked", "0,3\n", vec![vec![0]]),
        ("none blocked", "", vec![vec![0]]),
    ] {
        let temp = TempTree::new("antijoin-key-only");
        let program = temp.program(
            ".in
.decl candidate(f: number, l: number)
.input candidate.facts
.decl blocked(f: number, l: number)
.input blocked.facts
.printsize
.decl unblocked(f: number)
.rule
unblocked(F) :- candidate(F, L), !blocked(F, L).
",
        );
        temp.facts("candidate", "0,3\n0,4\n");
        temp.facts("blocked", blocked);

        assert_success(&run(&temp, &program, &[]));
        assert_eq!(rows(&temp.output("unblocked")), expected, "{label}");
    }
}

#[test]
fn an_antijoin_inside_a_recursive_stratum_answers_per_solution() {
    let temp = TempTree::new("antijoin-recursive");
    let program = temp.program(
        ".in
.decl seed(f: number, l: number)
.input seed.facts
.decl blocked(f: number, l: number)
.input blocked.facts
.decl step(f: number, g: number)
.input step.facts
.printsize
.decl live(f: number, l: number)
.decl reach(f: number)
.rule
live(F, L) :- seed(F, L).
live(G, L) :- live(F, L), step(F, G).
reach(F) :- live(F, L), !blocked(F, L).
",
    );
    temp.facts("seed", "0,3\n0,4\n");
    temp.facts("step", "0,1\n");
    // Every pair the fixed point reaches is blocked except (1, 4).
    temp.facts("blocked", "0,3\n0,4\n1,3\n");

    assert_success(&run(&temp, &program, &[]));
    assert_eq!(
        rows(&temp.output("live")),
        vec![vec![0, 3], vec![0, 4], vec![1, 3], vec![1, 4]],
    );
    assert_eq!(rows(&temp.output("reach")), vec![vec![1]]);
}

#[test]
fn an_antijoin_inside_a_recursive_stratum_blocks_every_solution() {
    let temp = TempTree::new("antijoin-recursive-blocked");
    let program = temp.program(
        ".in
.decl seed(f: number, l: number)
.input seed.facts
.decl blocked(f: number, l: number)
.input blocked.facts
.decl step(f: number, g: number)
.input step.facts
.printsize
.decl live(f: number, l: number)
.decl reach(f: number)
.rule
live(F, L) :- seed(F, L).
live(G, L) :- live(F, L), step(F, G).
reach(F) :- live(F, L), !blocked(F, L).
",
    );
    temp.facts("seed", "0,3\n0,4\n");
    temp.facts("step", "0,1\n");
    temp.facts("blocked", "0,3\n0,4\n1,3\n1,4\n");

    assert_success(&run(&temp, &program, &[]));
    assert_eq!(rows(&temp.output("reach")), Vec::<Vec<i64>>::new());
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

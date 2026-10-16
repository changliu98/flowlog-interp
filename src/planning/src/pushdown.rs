//! Filter calls, run where their arguments are bound.
//!
//! A rule's embedded calls run in its row program, after every join of its
//! body. A boolean filter call whose arguments the first few atoms of the body
//! already bind would discard rows that the later joins otherwise re-key,
//! arrange and multiply first. One rule of this shape joined two relations on
//! a column holding a single value, kept every one of the 284 million pairs
//! through two more joins, and only then applied the filter that left 24 rows:
//! three minutes and 44 GB.
//!
//! The rewrite cuts such a rule after the shortest prefix of its core atoms
//! that binds a filter's arguments - the atoms the rule's plan joins first,
//! in the order it joins them. The prefix, with every subsumed atom,
//! negation and comparison it alone binds, becomes a sideways slice whose
//! head keeps only the variables the rest of the rule reads; the rest of the
//! rule reads the slice in the prefix's place. The join of the prefix then
//! streams through the filter, so nothing it discards is ever arranged. A cut
//! is made only where the prefix joins two or more core atoms and at least one
//! core atom is joined after it: a single atom is filtered on its own by the
//! row program's cost anyway, and a prefix that is the whole body saves nothing.
//!
//! A filter that runs early sees rows the later joins would have discarded,
//! and a function may refuse those (an embedded block faults on purpose on a
//! value it cannot compute with). So the slice runs a *prefilter* - false
//! drops the row, a fault keeps it - and the filter itself stays where the
//! rule wrote it: a row that gets there is checked again, and faults exactly
//! when it would have without the rewrite. A comparison that can fault
//! (division or remainder) stays where it was written.
//!
//! Rules whose joins the optimizer orders (`-O2`, `.plan`) are not cut: their
//! plan does not join core atoms in the order the rule lists them.
//!
//! The rewrite is planning only: the program's rules, their canonical text and
//! every cache key are unchanged, and the slices are named after the rule so
//! that no two rules assembled into one dataflow share one.

use std::collections::HashSet;

use parsing::arithmetic::{Arithmetic, ArithmeticOperator};
use parsing::head::{Head, HeadArg};
use parsing::rule::{Atom, AtomArg, CallExpr, CallPredicate, FLRule, Predicate};

/// The rule, cut into slices and a final rule, or `None` when no filter call
/// can run before the rule's last join. Slices are listed by level: a slice of
/// level `n` reads the slice of level `n - 1` of the same rule.
pub fn push_down_filter_calls(rule: &FLRule, rule_identifier: usize) -> Option<(Vec<FLRule>, FLRule)> {
    let mut slices = Vec::new();
    let mut current = rule.clone();
    let mut level = 0;
    // Filters a slice already runs early; the rest keeps them, to recheck.
    let mut pushed: HashSet<String> = HashSet::new();
    while let Some((slice, rest)) = cut_once(&current, rule_identifier, level, &mut pushed) {
        slices.push(slice);
        current = rest;
        level += 1;
    }
    (!slices.is_empty()).then_some((slices, current))
}

fn atom_variables(atom: &Atom) -> HashSet<&String> {
    atom.arguments()
        .iter()
        .filter_map(|argument| match argument {
            AtomArg::Var(variable) => Some(variable),
            _ => None,
        })
        .collect()
}

fn call_variables(call: &CallExpr) -> HashSet<&String> {
    call.arguments()
        .iter()
        .filter_map(|argument| match argument {
            AtomArg::Var(variable) => Some(variable),
            _ => None,
        })
        .collect()
}

/// Whether evaluating `arithmetic` can fault: a division or a remainder.
fn can_fault(arithmetic: &Arithmetic) -> bool {
    arithmetic
        .rest()
        .iter()
        .any(|(operator, _)| matches!(operator, ArithmeticOperator::Divide | ArithmeticOperator::Modulo))
}

/// Whether each positive atom is a core atom: one whose variables are not all
/// among another atom's (the planner's rule, so the counts agree with its plan).
fn core_atoms(variables: &[HashSet<&String>]) -> Vec<bool> {
    let mut core = vec![true; variables.len()];
    for (i, mine) in variables.iter().enumerate() {
        for (j, other) in variables.iter().enumerate() {
            if i != j && mine.is_subset(other) {
                if mine.len() < other.len() {
                    core[i] = false;
                } else {
                    core[i.max(j)] = false;
                }
            }
        }
    }
    core
}

/// One cut: the slice of the shortest useful prefix, and the rule reading it.
fn cut_once(
    rule: &FLRule,
    rule_identifier: usize,
    level: usize,
    pushed: &mut HashSet<String>,
) -> Option<(FLRule, FLRule)> {
    let rhs = rule.rhs();
    let atoms: Vec<&Atom> = rhs
        .iter()
        .filter_map(|predicate| match predicate {
            Predicate::AtomPredicate(atom) => Some(atom),
            _ => None,
        })
        .collect();
    let atom_vars: Vec<HashSet<&String>> = atoms.iter().map(|atom| atom_variables(atom)).collect();
    let core = core_atoms(&atom_vars);
    let core_total = core.iter().filter(|&&is_core| is_core).count();
    let relational: HashSet<&String> = atom_vars.iter().flatten().copied().collect();

    // The prefix each filter needs, when it is a useful cut: the core atoms
    // before `length`. A subsumed atom binds nothing the plan has not bound.
    let mut best: Option<usize> = None;
    for predicate in rhs {
        let Predicate::CallPredicate(CallPredicate::Filter(call)) = predicate else {
            continue;
        };
        let arguments = call_variables(call);
        if arguments.is_empty() || !arguments.is_subset(&relational) || pushed.contains(&call.to_string()) {
            continue;
        }
        let mut bound: HashSet<&String> = HashSet::new();
        let mut cores = 0;
        for (position, variables) in atom_vars.iter().enumerate() {
            if !core[position] {
                continue;
            }
            bound.extend(variables.iter().copied());
            cores += 1;
            if arguments.is_subset(&bound) {
                if cores >= 2 && core_total - cores >= 1 {
                    best = Some(best.map_or(position + 1, |length: usize| length.min(position + 1)));
                }
                break;
            }
        }
    }
    let prefix_length = best?;
    let in_slice_atom = |position: usize| core[position] && position < prefix_length;

    let prefix_vars: HashSet<&String> = (0..prefix_length)
        .filter(|&position| in_slice_atom(position))
        .flat_map(|position| atom_vars[position].iter().copied())
        .collect();
    let in_prefix = |variables: &HashSet<&String>| variables.is_subset(&prefix_vars);

    // Partition the body: what the prefix alone binds goes into the slice.
    let mut slice_body = Vec::new();
    let mut rest_body = Vec::new();
    let mut atom_position = 0;
    for predicate in rhs {
        match predicate {
            Predicate::AtomPredicate(_) => {
                let position = atom_position;
                atom_position += 1;
                if in_slice_atom(position) || (!core[position] && in_prefix(&atom_vars[position])) {
                    slice_body.push(predicate.clone());
                } else {
                    rest_body.push(predicate.clone());
                }
            }
            Predicate::NegatedAtomPredicate(atom) if in_prefix(&atom_variables(atom)) => {
                slice_body.push(predicate.clone());
            }
            Predicate::ComparePredicate(comparison)
                if in_prefix(&comparison.vars_set())
                    && !can_fault(comparison.left())
                    && !can_fault(comparison.right()) =>
            {
                slice_body.push(predicate.clone());
            }
            Predicate::CallPredicate(CallPredicate::Filter(call)) => {
                let arguments = call_variables(call);
                let text = call.to_string();
                if !arguments.is_empty() && in_prefix(&arguments) && !pushed.contains(&text) {
                    slice_body.push(Predicate::CallPredicate(CallPredicate::Prefilter(call.clone())));
                    pushed.insert(text);
                }
                rest_body.push(predicate.clone());
            }
            _ => rest_body.push(predicate.clone()),
        }
    }

    // The slice keeps the prefix variables the rest of the rule reads.
    let mut needed: HashSet<&String> = HashSet::new();
    for argument in rule.head().head_arguments() {
        needed.extend(argument.vars());
    }
    for predicate in &rest_body {
        match predicate {
            Predicate::AtomPredicate(atom) | Predicate::NegatedAtomPredicate(atom) => {
                needed.extend(atom_variables(atom));
            }
            Predicate::ComparePredicate(comparison) => needed.extend(comparison.vars_set()),
            Predicate::CallPredicate(call) => needed.extend(call_variables(call.call())),
        }
    }
    let mut kept: Vec<String> = Vec::new();
    for (position, atom) in atoms.iter().enumerate().take(prefix_length) {
        if !in_slice_atom(position) {
            continue;
        }
        for argument in atom.arguments() {
            if let AtomArg::Var(variable) = argument {
                if needed.contains(variable) && !kept.contains(variable) {
                    kept.push(variable.clone());
                }
            }
        }
    }
    if kept.is_empty() {
        return None;
    }

    let name = format!(
        "__flowlog_pushdown__{}__{}_{}",
        rule.head().name(),
        rule_identifier,
        level
    );
    let slice = FLRule::new(
        Head::new(name.clone(), kept.iter().cloned().map(HeadArg::Var).collect()),
        slice_body,
        rule.is_planning(),
        false,
    )
    .at_line(rule.line())
    .sideways();

    let mut body = vec![Predicate::AtomPredicate(Atom::from_str(
        &name,
        kept.into_iter().map(AtomArg::Var).collect(),
    ))];
    body.extend(rest_body);
    let rest = FLRule::new(rule.head().clone(), body, rule.is_planning(), false).at_line(rule.line());
    Some((slice, rest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use parsing::parser::Program;

    fn rule(text: &str) -> FLRule {
        let source = format!(
            ".code rust\npub fn f(a: i64, b: i64, c: i64) -> bool {{ a + b == c }}\n.endcode\n\
             .in\n.decl i(b: number, c: number, a: number, s: number, k: number)\n.input i.facts\n\
             .decl r(t: number, p: number, x: number)\n.input r.facts\n\
             .decl q(t: number)\n.input q.facts\n\
             .decl p(x: number, y: number)\n.input p.facts\n\
             .printsize\n.decl h(c: number, t: number, a: number)\n\
             .rule\n{text}\n"
        );
        Program::parse(&source, "test.dl").expect("parses").rules()[0].clone()
    }

    #[test]
    fn a_filter_bound_by_the_first_join_runs_on_it() {
        let original = rule(
            "h(C, T, A) :- i(B, C, A, S, 1), i(B, T, N, _, 2), r(T, 0, X), q(T), @call(f, A, S, N).",
        );
        let (slices, rest) = push_down_filter_calls(&original, 7).expect("one cut");
        assert_eq!(slices.len(), 1);
        let slice = slices[0].to_string();
        // The slice keeps the filter's arguments: the rest checks it again.
        assert!(slice.starts_with("__flowlog_pushdown__h__7_0(C, A, S, T, N) :- "), "{slice}");
        assert!(slice.contains("@call(f, A, S, N)?"), "{slice}");
        // q(T) is subsumed and bound by the prefix: it filters inside the slice.
        assert!(slice.contains("q(T)"), "{slice}");
        assert!(slices[0].is_sideways());
        assert_eq!(
            rest.to_string(),
            "h(C, T, A) :- __flowlog_pushdown__h__7_0(C, A, S, T, N), r(T, 0, X), @call(f, A, S, N)."
        );
        assert!(!rest.is_sideways());
    }

    #[test]
    fn a_filter_needing_the_whole_body_stays_in_the_row_program() {
        let original = rule("h(C, T, A) :- i(B, C, A, S, 1), r(T, B, X), @call(f, A, S, X).");
        assert!(push_down_filter_calls(&original, 0).is_none());
    }

    #[test]
    fn a_filter_on_one_atom_is_left_alone() {
        let original = rule("h(C, T, A) :- i(B, C, A, S, 1), r(T, B, X), @call(f, A, S, C).");
        assert!(push_down_filter_calls(&original, 0).is_none());
    }

    #[test]
    fn a_subsumed_atom_the_prefix_does_not_bind_stays_with_its_subsumer() {
        // q(X) is subsumed by r(T, 0, X): in the slice it would be a cross
        // product with the prefix join.
        let original = rule(
            "h(C, T, A) :- q(X), i(B, C, A, S, 1), i(B, T, N, _, 2), r(T, 0, X), @call(f, A, S, N).",
        );
        let (slices, rest) = push_down_filter_calls(&original, 0).expect("one cut");
        assert!(!slices[0].to_string().contains("q(X)"), "{}", slices[0]);
        assert!(rest.to_string().contains("q(X)"), "{rest}");
    }

    #[test]
    fn a_comparison_that_can_fault_stays_where_it_was_written() {
        let original = rule(
            "h(C, T, A) :- i(B, C, A, S, 1), i(B, T, N, _, 2), r(T, 0, X), @call(f, A, S, N), \
             A + S > N, A / S > N.",
        );
        let (slices, rest) = push_down_filter_calls(&original, 0).expect("one cut");
        let slice = slices[0].to_string();
        assert!(slice.contains("A + S > N") && !slice.contains("A / S"), "{slice}");
        assert!(rest.to_string().contains("A / S > N"), "{rest}");
    }

    #[test]
    fn each_filter_runs_early_once_and_is_checked_again_where_it_was_written() {
        let original = rule(
            "h(C, T, A) :- i(B, C, A, S, 1), i(B, T, N, _, 2), r(T, 0, X), p(X, Y), \
             @call(f, A, S, N), @call(f, A, X, N).",
        );
        let (slices, rest) = push_down_filter_calls(&original, 0).expect("two cuts");
        assert_eq!(slices.len(), 2);
        let (first, second) = (slices[0].to_string(), slices[1].to_string());
        assert!(first.contains("@call(f, A, S, N)?") && !first.contains("@call(f, A, X, N)"), "{first}");
        assert!(second.contains("@call(f, A, X, N)?") && !second.contains("@call(f, A, S, N)"), "{second}");
        assert_eq!(
            rest.to_string(),
            "h(C, T, A) :- __flowlog_pushdown__h__0_1(C, A, S, T, N, X), p(X, Y), \
             @call(f, A, S, N), @call(f, A, X, N)."
        );
    }
}

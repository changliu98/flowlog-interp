//! Canonical text of a rule set: what the rules mean, not how they were written.
//!
//! A cache keyed on rule text invalidates on every spelling: a renamed variable,
//! a reordered body, a duplicated clause, a `.plan` hint.  None of those change
//! the rows a rule derives, so the cache key is built from this rendering
//! instead.  Variables are numbered by first appearance over the head and then
//! the body; body predicates are ordered by a name-free structural key, and
//! where several predicates tie on it every arrangement of the tied ones is
//! rendered and the smallest text wins, so two spellings of one rule reach one
//! text.  Planning hints are omitted: they steer the optimizer, not the answer.
//!
//! The rendering is sound for caching in the direction that matters: two rules
//! with the same text are alpha-equivalent reorderings of one another, so they
//! derive the same rows from the same inputs.  It is not complete -- two rules
//! may mean the same thing and still render differently -- and a miss there
//! costs one ordinary evaluation.

use parsing::arithmetic::{Arithmetic, Factor};
use parsing::compare::ComparisonExpr;
use parsing::head::HeadArg;
use parsing::rule::{Atom, AtomArg, CallPredicate, Const, FLRule, Predicate};
use std::collections::HashMap;

/// Upper bound on body arrangements tried for one rule.  Ties among body
/// predicates are rare and small; a rule beyond this budget is rendered in its
/// structurally sorted order, which is still name-free.
const ARRANGEMENT_BUDGET: usize = 5040;

/// The canonical text of a rule set: one line per distinct rule, sorted.
pub fn canonical_rules(rules: &[&FLRule]) -> String {
    let mut forms = rules
        .iter()
        .map(|rule| canonical_rule(rule))
        .collect::<Vec<_>>();
    forms.sort();
    forms.dedup();
    forms.join("\n")
}

/// The canonical text of one rule.
pub fn canonical_rule(rule: &FLRule) -> String {
    let body = rule.rhs();
    let keys = body.iter().map(structural_key).collect::<Vec<_>>();
    let mut order = (0..body.len()).collect::<Vec<_>>();
    order.sort_by(|a, b| keys[*a].cmp(&keys[*b]).then(a.cmp(b)));

    let mut groups: Vec<Vec<usize>> = Vec::new();
    for &index in &order {
        match groups.last_mut() {
            Some(group) if keys[group[0]] == keys[index] => group.push(index),
            _ => groups.push(vec![index]),
        }
    }

    let arrangements = groups
        .iter()
        .map(|group| factorial(group.len()))
        .try_fold(1usize, |product, count| product.checked_mul(count))
        .unwrap_or(usize::MAX);
    if arrangements <= 1 || arrangements > ARRANGEMENT_BUDGET {
        return render_rule(rule, &order);
    }

    let mut best: Option<String> = None;
    let mut chosen = Vec::with_capacity(order.len());
    for_each_arrangement(&groups, 0, &mut chosen, &mut |arrangement| {
        let form = render_rule(rule, arrangement);
        if best.as_ref().map_or(true, |current| form < *current) {
            best = Some(form);
        }
    });
    best.expect("at least one arrangement is rendered")
}

fn factorial(n: usize) -> usize {
    (1..=n).try_fold(1usize, |product, k| product.checked_mul(k)).unwrap_or(usize::MAX)
}

/// Visit every concatenation of one permutation per tie group.
fn for_each_arrangement(
    groups: &[Vec<usize>],
    depth: usize,
    chosen: &mut Vec<usize>,
    visit: &mut dyn FnMut(&[usize]),
) {
    if depth == groups.len() {
        visit(chosen);
        return;
    }
    let mut group = groups[depth].clone();
    let width = group.len();
    // Lexicographic permutation generation over the tie group.
    group.sort_unstable();
    loop {
        let mark = chosen.len();
        chosen.extend_from_slice(&group);
        for_each_arrangement(groups, depth + 1, chosen, visit);
        chosen.truncate(mark);
        if !next_permutation(&mut group[..width]) {
            break;
        }
    }
}

fn next_permutation(items: &mut [usize]) -> bool {
    if items.len() < 2 {
        return false;
    }
    let mut i = items.len() - 1;
    while i > 0 && items[i - 1] >= items[i] {
        i -= 1;
    }
    if i == 0 {
        return false;
    }
    let mut j = items.len() - 1;
    while items[j] <= items[i - 1] {
        j -= 1;
    }
    items.swap(i - 1, j);
    items[i..].reverse();
    true
}

/// Numbers variables by first appearance.
#[derive(Default)]
struct Namer {
    names: HashMap<String, usize>,
}

impl Namer {
    fn var(&mut self, name: &str) -> String {
        let next = self.names.len();
        let index = *self.names.entry(name.to_string()).or_insert(next);
        format!("v{index}")
    }
}

fn render_rule(rule: &FLRule, order: &[usize]) -> String {
    let mut namer = Namer::default();
    let head = rule.head();
    let head_arguments = head
        .head_arguments()
        .iter()
        .map(|argument| render_head_arg(argument, &mut namer))
        .collect::<Vec<_>>()
        .join(", ");
    let body = order
        .iter()
        .map(|&index| render_predicate(&rule.rhs()[index], &mut namer))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}({head_arguments}) :- {body}.", head.name())
}

fn render_head_arg(argument: &HeadArg, namer: &mut Namer) -> String {
    match argument {
        HeadArg::Var(name) => namer.var(name),
        HeadArg::Arith(arithmetic) => render_arithmetic(arithmetic, namer),
        HeadArg::Aggregation(aggregation) => format!(
            "{}({})",
            aggregation.operator(),
            render_arithmetic(aggregation.arithmetic(), namer)
        ),
    }
}

fn render_arithmetic(arithmetic: &Arithmetic, namer: &mut Namer) -> String {
    let mut text = render_factor(arithmetic.init(), namer);
    for (operator, factor) in arithmetic.rest() {
        text.push(' ');
        text.push_str(&operator.to_string());
        text.push(' ');
        text.push_str(&render_factor(factor, namer));
    }
    text
}

fn render_factor(factor: &Factor, namer: &mut Namer) -> String {
    match factor {
        Factor::Var(name) => namer.var(name),
        Factor::Const(constant) => render_const(constant),
    }
}

fn render_const(constant: &Const) -> String {
    match constant {
        Const::Integer(value) => format!("#{value}"),
        Const::Text(text) => format!("{text:?}"),
    }
}

fn render_predicate(predicate: &Predicate, namer: &mut Namer) -> String {
    match predicate {
        Predicate::AtomPredicate(atom) => render_atom(atom, namer),
        Predicate::NegatedAtomPredicate(atom) => format!("!{}", render_atom(atom, namer)),
        Predicate::ComparePredicate(comparison) => render_comparison(comparison, namer),
        Predicate::CallPredicate(call) => render_call(call, namer),
    }
}

fn render_atom(atom: &Atom, namer: &mut Namer) -> String {
    let arguments = atom
        .arguments()
        .iter()
        .map(|argument| render_atom_arg(argument, namer))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}({arguments})", atom.name())
}

fn render_atom_arg(argument: &AtomArg, namer: &mut Namer) -> String {
    match argument {
        AtomArg::Var(name) => namer.var(name),
        AtomArg::Const(constant) => render_const(constant),
        AtomArg::Placeholder => "_".to_string(),
    }
}

fn render_comparison(comparison: &ComparisonExpr, namer: &mut Namer) -> String {
    format!(
        "[{} {:?} {}]",
        render_arithmetic(comparison.left(), namer),
        comparison.operator(),
        render_arithmetic(comparison.right(), namer)
    )
}

fn render_call(call: &CallPredicate, namer: &mut Namer) -> String {
    let expression = call.call();
    let arguments = expression
        .arguments()
        .iter()
        .map(|argument| render_atom_arg(argument, namer))
        .collect::<Vec<_>>()
        .join(", ");
    let text = format!("@call({}; {arguments})", expression.function());
    match call.output() {
        Some(output) => format!("{} = {text}", namer.var(output)),
        None => text,
    }
}

/// A name-free rendering of one predicate, used only to order the body.
fn structural_key(predicate: &Predicate) -> String {
    let mut blind = Namer::default();
    let text = match predicate {
        Predicate::AtomPredicate(atom) => format!("A:{}", blind_atom(atom)),
        Predicate::NegatedAtomPredicate(atom) => format!("N:{}", blind_atom(atom)),
        Predicate::ComparePredicate(comparison) => format!(
            "C:[{} {:?} {}]",
            blind_arithmetic(comparison.left()),
            comparison.operator(),
            blind_arithmetic(comparison.right())
        ),
        Predicate::CallPredicate(call) => {
            let expression = call.call();
            let arguments = expression
                .arguments()
                .iter()
                .map(blind_atom_arg)
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "F:{}:@call({}; {arguments})",
                call.output().map_or("", |_| "bind"),
                expression.function()
            )
        }
    };
    let _ = &mut blind;
    text
}

fn blind_atom(atom: &Atom) -> String {
    let arguments = atom
        .arguments()
        .iter()
        .map(blind_atom_arg)
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}({arguments})", atom.name())
}

fn blind_atom_arg(argument: &AtomArg) -> String {
    match argument {
        AtomArg::Var(_) => "?".to_string(),
        AtomArg::Const(constant) => render_const(constant),
        AtomArg::Placeholder => "_".to_string(),
    }
}

fn blind_arithmetic(arithmetic: &Arithmetic) -> String {
    let blind_factor = |factor: &Factor| match factor {
        Factor::Var(_) => "?".to_string(),
        Factor::Const(constant) => render_const(constant),
    };
    let mut text = blind_factor(arithmetic.init());
    for (operator, factor) in arithmetic.rest() {
        text.push(' ');
        text.push_str(&operator.to_string());
        text.push(' ');
        text.push_str(&blind_factor(factor));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use parsing::parser::Program;

    fn rules(source: &str) -> Vec<FLRule> {
        let program = Program::from_source(
            &format!(
                ".in\n.decl A(x: number, y: number)\n.input A.facts\n\
                 .decl B(x: number, y: number)\n.input B.facts\n\
                 .printsize\n.decl H(x: number, y: number)\n.decl C(x: number)\n.rule\n{source}\n"
            ),
            "canonical-test",
        );
        program.rules().clone()
    }

    #[test]
    fn renaming_variables_does_not_change_the_text() {
        let a = rules("H(x, y) :- A(x, z), B(z, y).");
        let b = rules("H(p, q) :- A(p, r), B(r, q).");
        assert_eq!(canonical_rule(&a[0]), canonical_rule(&b[0]));
    }

    #[test]
    fn reordering_the_body_does_not_change_the_text() {
        let a = rules("H(x, y) :- A(x, z), B(z, y).");
        let b = rules("H(x, y) :- B(z, y), A(x, z).");
        assert_eq!(canonical_rule(&a[0]), canonical_rule(&b[0]));
    }

    #[test]
    fn tied_predicates_reach_one_text() {
        let a = rules("H(x, z) :- A(x, y), A(y, z).");
        let b = rules("H(p, r) :- A(q, r), A(p, q).");
        assert_eq!(canonical_rule(&a[0]), canonical_rule(&b[0]));
    }

    #[test]
    fn a_different_join_is_a_different_text() {
        let a = rules("H(x, z) :- A(x, y), A(y, z).");
        let b = rules("H(x, z) :- A(x, y), A(z, y).");
        assert_ne!(canonical_rule(&a[0]), canonical_rule(&b[0]));
    }

    #[test]
    fn planning_hints_and_duplicates_do_not_change_a_rule_set() {
        let a = rules("H(x, y) :- A(x, z), B(z, y).");
        let b = rules("H(x, y) :- A(x, z), B(z, y). .plan\nH(a, b) :- B(c, b), A(a, c).");
        let a_refs = a.iter().collect::<Vec<_>>();
        let b_refs = b.iter().collect::<Vec<_>>();
        assert_eq!(canonical_rules(&a_refs), canonical_rules(&b_refs));
    }

    #[test]
    fn negation_constants_and_comparisons_are_kept_apart() {
        let a = rules("C(x) :- A(x, 1), !B(x, y), x < y.");
        let b = rules("C(x) :- A(x, 2), !B(x, y), x < y.");
        let c = rules("C(x) :- A(x, 1), B(x, y), x < y.");
        let d = rules("C(x) :- A(x, 1), !B(x, y), y < x.");
        let forms = [&a[0], &b[0], &c[0], &d[0]]
            .iter()
            .map(|rule| canonical_rule(rule))
            .collect::<Vec<_>>();
        for i in 0..forms.len() {
            for j in 0..forms.len() {
                assert_eq!(i == j, forms[i] == forms[j], "{} vs {}", forms[i], forms[j]);
            }
        }
    }
}

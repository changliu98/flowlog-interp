//! Program-level well-formedness checks that the grammar cannot express.
//!
//! The grammar accepts several shapes the evaluator has no plan for: a constant
//! or an arithmetic expression in a rule head, an expression inside an
//! aggregate, a head whose arity disagrees with its `.decl`, two rules that
//! derive one relation under different aggregation operators, and a `string`
//! column. Each of those used to run and quietly answer a different query - a
//! head constant became a narrower projection, an aggregated expression
//! aggregated its first variable, the first aggregation operator won for every
//! rule of the relation, a `string` column loaded no rows at all.
//!
//! A silently wrong answer is worse than a refusal, so a program carrying one
//! of these shapes is refused here, before stratification and before any
//! dataflow is assembled. Nothing in this module implements a feature; it only
//! reports what this engine version does not implement, quoting the rule.

use std::collections::hash_map::Entry;
use std::collections::HashMap;

use crate::aggregation::AggregationOperator;
use crate::decl::DataType;
use crate::head::HeadArg;
use crate::parser::Program;
use crate::rule::FLRule;

/// Refuses a parsed program that the evaluator would answer incorrectly.
///
/// Panics with a message quoting the offending rule or declaration.
pub fn validate_program(program: &Program) {
    refuse_unimplemented_column_types(program);

    let declared_arity = declared_arities(program);

    for rule in program.rules() {
        refuse_unsupported_head_arguments(rule);
        refuse_head_arity_disagreement(rule, &declared_arity);
    }

    refuse_disagreeing_head_operators(program);
}

/// Every column of every relation is a `number`.
///
/// `string` parses, and nothing implements it: the value domain has no
/// representation for text, so a `string` column of an input relation matched
/// no cell and the relation loaded zero rows without a word anywhere. An
/// unimplemented type is a refusal, not an empty relation.
fn refuse_unimplemented_column_types(program: &Program) {
    for declaration in program.edbs().iter().chain(program.idbs().iter()) {
        for attribute in declaration.attributes() {
            if matches!(attribute.data_type(), DataType::String) {
                panic!(
                    "relation {declaration} declares column {:?} as `string`, and string \
                     columns are not implemented in this engine version: every value is a \
                     `number`",
                    attribute.name()
                );
            }
        }
    }
}

/// Every relation arity stated by a `.decl`, input or output.
fn declared_arities(program: &Program) -> HashMap<&str, usize> {
    program
        .edbs()
        .iter()
        .chain(program.idbs().iter())
        .map(|declaration| (declaration.name(), declaration.arity()))
        .collect()
}

/// A head argument is either a variable, or an aggregate over a variable.
///
/// Anything else - `R(x, 7)`, `R(x + y)`, `R(k, sum(v + 1))` - is a value the
/// engine cannot compute at head position: planning flattens a head to the
/// variables it mentions, so the constant disappears and the expression
/// degenerates to its operands.
fn refuse_unsupported_head_arguments(rule: &FLRule) {
    for (index, argument) in rule.head().head_arguments().iter().enumerate() {
        let position = index + 1;
        match argument {
            HeadArg::Var(_) => {}
            HeadArg::Arith(arithmetic) => {
                // A single variable parses as `HeadArg::Var`, so an `Arith`
                // with no operator is a bare constant.
                let kind = if arithmetic.rest().is_empty() {
                    "constant"
                } else {
                    "arithmetic expression"
                };
                panic!(
                    "rule {rule} has the {kind} `{arithmetic}` in head position {position}: \
                     head constants and head arithmetic are not supported by this engine \
                     version. Bind the value in the rule body and name the variable in the head."
                );
            }
            HeadArg::Aggregation(aggregation) => {
                if !aggregation.arithmetic().is_var() {
                    panic!(
                        "rule {rule} aggregates the expression `{}` in head position \
                         {position}: an aggregate over anything but a single variable is not \
                         supported by this engine version. Derive the expression into a \
                         helper relation and aggregate that relation's column.",
                        aggregation.arithmetic()
                    );
                }
            }
        }
    }
}

/// A rule may not derive a relation at an arity its `.decl` does not have.
///
/// A relation with no `.decl` at all is left alone: it is neither read from
/// disk nor written out, so there is no declared arity to disagree with.
fn refuse_head_arity_disagreement(rule: &FLRule, declared_arity: &HashMap<&str, usize>) {
    let head = rule.head();
    let name = head.name();
    let Some(&declared) = declared_arity.get(name.as_str()) else {
        return;
    };
    if head.arity() != declared {
        panic!(
            "rule {rule} derives {name} with {} head argument(s), but {name} is declared with \
             arity {declared}",
            head.arity()
        );
    }
}

/// Every rule deriving one relation must agree on how that relation is
/// aggregated.
///
/// The engine applies at most one aggregation per relation, taken from the
/// first aggregate rule it sees, so a disagreement is not a merge: the losing
/// rules are evaluated under an operator they did not ask for, and a plain rule
/// mixed with an aggregate rule has its rows aggregated as well.
fn refuse_disagreeing_head_operators(program: &Program) {
    let mut first_rule_per_head: HashMap<&str, (&FLRule, Option<AggregationOperator>)> =
        HashMap::new();

    for rule in program.rules() {
        let operator = head_aggregation_operator(rule);
        let name = rule.head().name().as_str();

        match first_rule_per_head.entry(name) {
            Entry::Vacant(slot) => {
                slot.insert((rule, operator));
            }
            Entry::Occupied(slot) => {
                let (earlier_rule, earlier_operator) = slot.get();
                match (earlier_operator, operator) {
                    (Some(earlier), Some(current)) if *earlier != current => panic!(
                        "relation {name} is derived by rules with different aggregation \
                         operators, `{earlier}` and `{current}`; every rule deriving one \
                         relation must agree, because the engine applies one aggregation to \
                         the whole relation.\nrule: {earlier_rule}\nrule: {rule}"
                    ),
                    (Some(_), None) | (None, Some(_)) => panic!(
                        "relation {name} is derived by both a plain rule and an aggregate \
                         rule; the engine aggregates the whole relation, so the plain rule's \
                         rows would be aggregated too.\nrule: {earlier_rule}\nrule: {rule}"
                    ),
                    _ => {}
                }
            }
        }
    }
}

/// The aggregation operator a rule head applies, if it aggregates at all.
///
/// The grammar admits an aggregate only as the last head argument.
fn head_aggregation_operator(rule: &FLRule) -> Option<AggregationOperator> {
    rule.head()
        .head_arguments()
        .last()
        .and_then(|argument| match argument {
            HeadArg::Aggregation(aggregation) => Some(*aggregation.operator()),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use crate::parser::Program;

    fn validate(source: &str) {
        Program::from_source(source, "validate-test.dl");
    }

    #[test]
    fn accepts_variables_and_aggregates_over_one_variable() {
        validate(
            ".in\n\
             .decl E(k: number, v: number)\n\
             .printsize\n\
             .decl R(k: number, v: number)\n\
             .decl S(k: number, v: number)\n\
             .rule\n\
             R(k, v) :- E(k, v).\n\
             S(k, min(v)) :- E(k, v).",
        );
    }

    #[test]
    #[should_panic(expected = "string columns are not implemented")]
    fn refuses_a_string_input_column() {
        validate(
            ".in\n\
             .decl E(k: number, v: string)\n\
             .printsize\n\
             .decl R(k: number)\n\
             .rule\n\
             R(k) :- E(k, v).",
        );
    }

    #[test]
    #[should_panic(expected = "string columns are not implemented")]
    fn refuses_a_string_output_column() {
        validate(
            ".in\n\
             .decl E(k: number, v: number)\n\
             .printsize\n\
             .decl R(k: number, v: string)\n\
             .rule\n\
             R(k, v) :- E(k, v).",
        );
    }

    #[test]
    #[should_panic(expected = "head constants and head arithmetic are not supported")]
    fn refuses_a_head_constant() {
        validate(
            ".in\n\
             .decl E(k: number, v: number)\n\
             .printsize\n\
             .decl R(k: number, v: number)\n\
             .rule\n\
             R(k, 7) :- E(k, v).",
        );
    }

    #[test]
    #[should_panic(expected = "head constants and head arithmetic are not supported")]
    fn refuses_head_arithmetic() {
        validate(
            ".in\n\
             .decl E(k: number, v: number)\n\
             .printsize\n\
             .decl R(s: number)\n\
             .rule\n\
             R(k + v) :- E(k, v).",
        );
    }

    #[test]
    #[should_panic(expected = "an aggregate over anything but a single variable")]
    fn refuses_an_expression_inside_an_aggregate() {
        validate(
            ".in\n\
             .decl E(k: number, v: number)\n\
             .printsize\n\
             .decl R(k: number, s: number)\n\
             .rule\n\
             R(k, sum(v + 100)) :- E(k, v).",
        );
    }

    #[test]
    #[should_panic(expected = "is declared with arity 3")]
    fn refuses_a_head_arity_that_disagrees_with_the_declaration() {
        validate(
            ".in\n\
             .decl E(k: number, v: number)\n\
             .printsize\n\
             .decl R(a: number, b: number, c: number)\n\
             .rule\n\
             R(k, v) :- E(k, v).",
        );
    }

    #[test]
    #[should_panic(expected = "different aggregation operators")]
    fn refuses_two_aggregation_operators_on_one_relation() {
        validate(
            ".in\n\
             .decl E(k: number, v: number)\n\
             .printsize\n\
             .decl R(k: number, a: number)\n\
             .rule\n\
             R(k, sum(v)) :- E(k, v).\n\
             R(k, max(v)) :- E(k, v).",
        );
    }

    #[test]
    #[should_panic(expected = "both a plain rule and an aggregate rule")]
    fn refuses_a_plain_rule_beside_an_aggregate_rule() {
        validate(
            ".in\n\
             .decl E(k: number, v: number)\n\
             .printsize\n\
             .decl R(k: number, a: number)\n\
             .rule\n\
             R(k, v) :- E(k, v).\n\
             R(k, sum(v)) :- E(k, v).",
        );
    }

    #[test]
    fn leaves_an_undeclared_head_alone() {
        validate(
            ".in\n\
             .decl E(k: number, v: number)\n\
             .printsize\n\
             .decl R(k: number, v: number)\n\
             .rule\n\
             Helper(k, v) :- E(k, v).\n\
             R(k, v) :- Helper(k, v).",
        );
    }
}

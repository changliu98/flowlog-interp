//! Program-level well-formedness and typing.
//!
//! The grammar accepts shapes the engine cannot evaluate: a rule with no
//! positive atom, a negated or compared variable nothing binds, a head
//! variable nothing binds, two rules that aggregate one relation differently,
//! a call to a function no block defines. Each used to be a panic somewhere
//! downstream, or worse, a quietly different query. They are refused here,
//! before stratification and before any dataflow is assembled, as
//! diagnostics that name the rule and its line.
//!
//! The same pass types the program. Every declared relation has column types;
//! every undeclared one gets them by inference over the rules that derive it,
//! to a fixed point, with an unconstrained column defaulting to `number`.
//! Then every rule is checked against the final types: a variable has one type
//! across the atoms that bind it, arithmetic and ordered comparison take
//! numbers, equality takes two of a kind, a call's arguments match its
//! parameters, and a head derives its relation's column types. What comes out
//! is the type of every column of every relation the program mentions, which
//! is what the engine reads symbol columns by.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use crate::aggregation::AggregationOperator;
use crate::arithmetic::{Arithmetic, Factor};
use crate::decl::{DataType, RelDecl};
use crate::diagnostic::{Diagnostic, Location, Result};
use crate::embedded::RustReturnType;
use crate::head::HeadArg;
use crate::parser::Program;
use crate::rule::{Atom, AtomArg, CallPredicate, Const, FLRule};

/// Refuse a program the engine would answer incorrectly, or type it.
pub fn validate_program(program: &Program) -> Result<HashMap<String, Vec<DataType>>> {
    let declared = declarations(program)?;
    let mut shapes = Vec::with_capacity(program.rules().len());
    for rule in program.rules() {
        shapes.push(check_shape(program, rule, &declared)?);
    }
    check_head_agreement(program)?;
    let arities = relation_arities(program, &declared)?;
    let types = infer_types(program, &declared, &arities, &shapes)?;
    for (rule, shape) in program.rules().iter().zip(shapes.iter()) {
        check_types(program, rule, shape, &types)?;
    }
    Ok(types)
}

/// A diagnostic about one rule: the rule text, its line, its head.
fn refuse(program: &Program, rule: &FLRule, message: String) -> Diagnostic {
    Diagnostic::validation(message)
        .with_rule(rule)
        .with_location(Location::new(program.name(), rule.line(), 0))
        .with_relation(rule.head().name().clone())
}

fn declarations(program: &Program) -> Result<HashMap<String, RelDecl>> {
    let mut declared: HashMap<String, RelDecl> = HashMap::new();
    for declaration in program.edbs().iter().chain(program.idbs().iter()) {
        match declared.entry(declaration.name().to_string()) {
            Entry::Vacant(slot) => {
                slot.insert(declaration.clone());
            }
            Entry::Occupied(earlier) => {
                return Err(Diagnostic::validation(format!(
                    "relation {} is declared twice: `{}` and `{}`",
                    declaration.name(),
                    earlier.get(),
                    declaration
                ))
                .with_relation(declaration.name())
                .with_location(Location::new(program.name(), declaration.line(), 0)));
            }
        }
    }
    Ok(declared)
}

/// What one rule binds, once its body has been checked.
struct Shape {
    /// Variables bound by call results, with the type of each.
    call_outputs: HashMap<String, DataType>,
}

fn atom_vars(atom: &Atom) -> impl Iterator<Item = &String> {
    atom.arguments().iter().filter_map(|argument| match argument {
        AtomArg::Var(variable) => Some(variable),
        _ => None,
    })
}

fn check_shape(program: &Program, rule: &FLRule, declared: &HashMap<String, RelDecl>) -> Result<Shape> {
    let positives = rule.positive_atoms().collect::<Vec<_>>();
    if positives.is_empty() {
        return Err(refuse(
            program,
            rule,
            format!(
                "rule {rule} has no positive relational atom in its body; every rule needs at \
                 least one to drive its evaluation"
            ),
        ));
    }

    let positive_vars = positives
        .iter()
        .flat_map(|atom| atom_vars(atom).cloned())
        .collect::<HashSet<String>>();

    for atom in positives.iter().copied().chain(rule.negated_atoms()) {
        if let Some(declaration) = declared.get(atom.name()) {
            if declaration.arity() != atom.arity() {
                return Err(refuse(
                    program,
                    rule,
                    format!(
                        "rule {rule} reads {} with {} argument(s), but {} is declared with arity {}",
                        atom.name(),
                        atom.arity(),
                        atom.name(),
                        declaration.arity()
                    ),
                )
                .with_relation(atom.name()));
            }
        }
    }

    for atom in rule.negated_atoms() {
        // An antijoin removes the rows that agree on shared columns, so a
        // negated atom retaining none has nothing to agree on: `!t(_, _)`
        // asks whether `t` has any row at all, a test on the whole relation
        // rather than a join against it.
        if atom_vars(atom).next().is_none() {
            return Err(refuse(
                program,
                rule,
                format!(
                    "rule {rule} negates {} without retaining any of its columns; a negated                      atom whose every position is a constant or a wildcard tests whether the                      whole relation is empty, which is not supported by this engine version",
                    atom.name()
                ),
            )
            .with_relation(atom.name()));
        }
        for variable in atom_vars(atom) {
            if !positive_vars.contains(variable) {
                return Err(refuse(
                    program,
                    rule,
                    format!(
                        "unsafe var detected at negation !{atom} of rule {rule}: variable \
                         {variable} is not bound by a positive atom"
                    ),
                ));
            }
        }
    }

    let mut call_outputs: HashMap<String, DataType> = HashMap::new();
    for call in rule.calls() {
        let expression = call.call();
        let name = expression.function();
        let Some(embedded) = program.embedded_rust() else {
            return Err(refuse(
                program,
                rule,
                format!("rule {rule} uses @call but the program has no .code rust section"),
            )
            .with_function(name));
        };
        let Some(function) = embedded.function(name) else {
            return Err(refuse(
                program,
                rule,
                format!("rule {rule} calls unknown embedded Rust function {name:?}"),
            )
            .with_function(name));
        };
        if function.arity() != expression.arguments().len() {
            return Err(refuse(
                program,
                rule,
                format!(
                    "rule {rule} calls {name:?} with {} arguments, but its signature is `{}`",
                    expression.arguments().len(),
                    function.signature()
                ),
            )
            .with_function(name));
        }
        for argument in expression.arguments() {
            match argument {
                AtomArg::Var(variable) => {
                    if !positive_vars.contains(variable) && !call_outputs.contains_key(variable) {
                        return Err(refuse(
                            program,
                            rule,
                            format!(
                                "rule {rule} calls {name:?} before variable {variable:?} is \
                                 bound by a positive atom or an earlier @call"
                            ),
                        )
                        .with_function(name));
                    }
                }
                AtomArg::Placeholder => {
                    return Err(refuse(
                        program,
                        rule,
                        format!("rule {rule} passes '_' to {name:?}; every @call input must be bound"),
                    )
                    .with_function(name));
                }
                AtomArg::Const(_) => {}
            }
        }
        match call {
            CallPredicate::Bind { output, .. } => {
                let Some(value_type) = function.return_type().value_type() else {
                    return Err(refuse(
                        program,
                        rule,
                        format!(
                            "rule {rule} binds bool-returning {name:?}; use it as a bare @call \
                             filter"
                        ),
                    )
                    .with_function(name));
                };
                if positive_vars.contains(output) || call_outputs.contains_key(output) {
                    return Err(refuse(
                        program,
                        rule,
                        format!(
                            "rule {rule} binds @call output {output:?} more than once or over a \
                             relational variable"
                        ),
                    )
                    .with_function(name));
                }
                call_outputs.insert(output.clone(), value_type);
            }
            CallPredicate::Filter(_) => {
                if function.return_type() != RustReturnType::Bool {
                    return Err(refuse(
                        program,
                        rule,
                        format!(
                            "rule {rule} uses {}-returning {name:?} as a filter; bind its result \
                             with 'Variable = @call(...)'",
                            function.return_type()
                        ),
                    )
                    .with_function(name));
                }
            }
        }
    }

    let bound = |variable: &String| positive_vars.contains(variable) || call_outputs.contains_key(variable);

    for comparison in rule.comparisons() {
        for variable in comparison.vars_set() {
            if !bound(variable) {
                return Err(refuse(
                    program,
                    rule,
                    format!("rule {rule} compares unbound variable {variable:?}"),
                ));
            }
        }
    }

    let head = rule.head();
    let aggregates = head
        .head_arguments()
        .iter()
        .filter(|argument| argument.is_aggregation())
        .count();
    if aggregates > 1 {
        return Err(refuse(
            program,
            rule,
            format!("rule {rule} has more than one aggregate in its head; a head aggregates at most one argument"),
        ));
    }
    for argument in head.head_arguments() {
        for variable in argument.vars() {
            if !bound(variable) {
                return Err(refuse(
                    program,
                    rule,
                    format!("rule {rule} emits unbound head variable {variable:?}"),
                ));
            }
        }
    }
    if let Some(declaration) = declared.get(head.name().as_str()) {
        if head.arity() != declaration.arity() {
            return Err(refuse(
                program,
                rule,
                format!(
                    "rule {rule} derives {} with {} head argument(s), but {} is declared with \
                     arity {}",
                    head.name(),
                    head.arity(),
                    head.name(),
                    declaration.arity()
                ),
            ));
        }
    }

    Ok(Shape { call_outputs })
}

/// Every rule deriving one relation must agree on how that relation is
/// aggregated: the same operator in the same position, or none at all.
///
/// The engine applies at most one aggregation per relation, so a disagreement
/// is not a merge: the losing rules would be evaluated under an operator they
/// did not ask for, and a plain rule mixed with an aggregate rule would have
/// its rows aggregated as well.
fn check_head_agreement(program: &Program) -> Result<()> {
    let mut first_rule_per_head: HashMap<&str, (&FLRule, Option<(AggregationOperator, usize)>)> =
        HashMap::new();

    for rule in program.rules() {
        let head = rule.head();
        let aggregate = head
            .aggregate_position()
            .and_then(|position| head.aggregation().map(|aggregation| (*aggregation.operator(), position)));
        let name = head.name().as_str();

        match first_rule_per_head.entry(name) {
            Entry::Vacant(slot) => {
                slot.insert((rule, aggregate));
            }
            Entry::Occupied(slot) => {
                let (earlier_rule, earlier) = slot.get();
                match (earlier, aggregate) {
                    (Some((earlier_operator, earlier_position)), Some((operator, position)))
                        if *earlier_operator != operator =>
                    {
                        let _ = (earlier_position, position);
                        return Err(refuse(
                            program,
                            rule,
                            format!(
                                "relation {name} is derived by rules with different aggregation \
                                 operators, `{earlier_operator}` and `{operator}`; every rule \
                                 deriving one relation must agree, because the engine applies \
                                 one aggregation to the whole relation.\nrule: {earlier_rule}\n\
                                 rule: {rule}"
                            ),
                        ));
                    }
                    (Some((_, earlier_position)), Some((_, position)))
                        if *earlier_position != position =>
                    {
                        return Err(refuse(
                            program,
                            rule,
                            format!(
                                "relation {name} is aggregated in column {} by one rule and in \
                                 column {} by another; every rule deriving one relation must \
                                 aggregate the same column.\nrule: {earlier_rule}\nrule: {rule}",
                                earlier_position + 1,
                                position + 1
                            ),
                        ));
                    }
                    (Some(_), None) | (None, Some(_)) => {
                        return Err(refuse(
                            program,
                            rule,
                            format!(
                                "relation {name} is derived by both a plain rule and an aggregate \
                                 rule; the engine aggregates the whole relation, so the plain \
                                 rule's rows would be aggregated too.\nrule: {earlier_rule}\n\
                                 rule: {rule}"
                            ),
                        ));
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// The arity of every relation the program mentions: declared, or agreed on
/// by every mention.
fn relation_arities(program: &Program, declared: &HashMap<String, RelDecl>) -> Result<HashMap<String, usize>> {
    let mut arities: HashMap<String, (usize, &FLRule)> = HashMap::new();
    let mut note = |program: &Program, name: &str, arity: usize, rule: &'_ FLRule| -> Result<()> {
        if declared.contains_key(name) {
            return Ok(());
        }
        match arities.get(name) {
            None => {
                // SAFETY of lifetimes: the map only lives within this function.
                let rule: &FLRule = unsafe { &*(rule as *const FLRule) };
                arities.insert(name.to_string(), (arity, rule));
                Ok(())
            }
            Some((known, first)) if *known != arity => Err(refuse(
                program,
                rule,
                format!(
                    "relation {name} is used with arity {known} in rule `{first}` and with \
                     arity {arity} in rule `{rule}`"
                ),
            )
            .with_relation(name)),
            Some(_) => Ok(()),
        }
    };
    for rule in program.rules() {
        note(program, rule.head().name(), rule.head().arity(), rule)?;
        for atom in rule.positive_atoms().chain(rule.negated_atoms()) {
            note(program, atom.name(), atom.arity(), rule)?;
        }
    }
    let mut result: HashMap<String, usize> = declared
        .iter()
        .map(|(name, declaration)| (name.clone(), declaration.arity()))
        .collect();
    for (name, (arity, _)) in arities {
        result.insert(name, arity);
    }
    Ok(result)
}

fn literal_type(constant: &Const) -> DataType {
    match constant {
        Const::Integer(_) => DataType::Integer,
        Const::Text(_) | Const::Symbol { .. } => DataType::Symbol,
    }
}

/// The type of every variable a rule binds, from the atoms that bind it and
/// the calls that compute it; refuses a variable typed two ways.
fn var_types(
    program: &Program,
    rule: &FLRule,
    shape: &Shape,
    types: &HashMap<String, Vec<Option<DataType>>>,
) -> Result<HashMap<String, DataType>> {
    let mut vars: HashMap<String, (DataType, String)> = HashMap::new();
    for atom in rule.positive_atoms().chain(rule.negated_atoms()) {
        let Some(columns) = types.get(atom.name()) else {
            continue;
        };
        for (position, argument) in atom.arguments().iter().enumerate() {
            let AtomArg::Var(variable) = argument else {
                continue;
            };
            let Some(column_type) = columns.get(position).copied().flatten() else {
                continue;
            };
            match vars.get(variable) {
                None => {
                    vars.insert(variable.clone(), (column_type, atom.to_string()));
                }
                Some((known, first)) if *known != column_type => {
                    return Err(refuse(
                        program,
                        rule,
                        format!(
                            "rule {rule}: variable {variable} is a {known} in {first} and a \
                             {column_type} in {atom}"
                        ),
                    )
                    .with_relation(atom.name()));
                }
                Some(_) => {}
            }
        }
    }
    let mut result: HashMap<String, DataType> = vars
        .into_iter()
        .map(|(variable, (column_type, _))| (variable, column_type))
        .collect();
    for (output, value_type) in &shape.call_outputs {
        result.insert(output.clone(), *value_type);
    }
    Ok(result)
}

/// The type of an expression, when every variable in it is typed. A single
/// factor has its own type; anything with an operator is a number.
fn expression_type(arithmetic: &Arithmetic, vars: &HashMap<String, DataType>) -> Option<DataType> {
    if arithmetic.is_single() {
        return match arithmetic.init() {
            Factor::Var(variable) => vars.get(variable).copied(),
            Factor::Const(constant) => Some(literal_type(constant)),
        };
    }
    Some(DataType::Integer)
}

fn head_arg_type(argument: &HeadArg, vars: &HashMap<String, DataType>) -> Option<DataType> {
    match argument {
        HeadArg::Var(variable) => vars.get(variable).copied(),
        HeadArg::Arith(arithmetic) => expression_type(arithmetic, vars),
        HeadArg::Aggregation(aggregation) => match aggregation.operator() {
            AggregationOperator::Count => Some(DataType::Integer),
            _ => expression_type(aggregation.arithmetic(), vars),
        },
    }
}

fn infer_types(
    program: &Program,
    declared: &HashMap<String, RelDecl>,
    arities: &HashMap<String, usize>,
    shapes: &[Shape],
) -> Result<HashMap<String, Vec<DataType>>> {
    let mut types: HashMap<String, Vec<Option<DataType>>> = arities
        .iter()
        .map(|(name, &arity)| {
            let columns = match declared.get(name) {
                Some(declaration) => declaration.column_types().into_iter().map(Some).collect(),
                None => vec![None; arity],
            };
            (name.clone(), columns)
        })
        .collect();

    loop {
        let mut changed = false;
        for (rule, shape) in program.rules().iter().zip(shapes.iter()) {
            let vars = var_types(program, rule, shape, &types)?;
            let head = rule.head();
            let derived = head
                .head_arguments()
                .iter()
                .map(|argument| head_arg_type(argument, &vars))
                .collect::<Vec<_>>();
            let columns = types
                .get_mut(head.name().as_str())
                .expect("every head relation has an arity");
            for (position, derived_type) in derived.into_iter().enumerate() {
                let Some(derived_type) = derived_type else {
                    continue;
                };
                match columns[position] {
                    None => {
                        columns[position] = Some(derived_type);
                        changed = true;
                    }
                    Some(existing) if existing != derived_type => {
                        return Err(refuse(
                            program,
                            rule,
                            format!(
                                "rule {rule} derives column {} of {} as a {derived_type}, but \
                                 that column is a {existing}",
                                position + 1,
                                head.name()
                            ),
                        ));
                    }
                    Some(_) => {}
                }
            }
        }
        if !changed {
            break;
        }
    }

    Ok(types
        .into_iter()
        .map(|(name, columns)| {
            (
                name,
                columns
                    .into_iter()
                    .map(|column| column.unwrap_or(DataType::Integer))
                    .collect(),
            )
        })
        .collect())
}

/// The type of an expression under complete typing, refusing arithmetic over
/// a symbol.
fn checked_expression_type(
    program: &Program,
    rule: &FLRule,
    arithmetic: &Arithmetic,
    vars: &HashMap<String, DataType>,
) -> Result<DataType> {
    if arithmetic.is_single() {
        return Ok(expression_type(arithmetic, vars).unwrap_or(DataType::Integer));
    }
    for factor in arithmetic.factors() {
        let factor_type = match factor {
            Factor::Var(variable) => vars.get(variable).copied().unwrap_or(DataType::Integer),
            Factor::Const(constant) => literal_type(constant),
        };
        if factor_type != DataType::Integer {
            return Err(refuse(
                program,
                rule,
                format!(
                    "rule {rule} computes `{arithmetic}` with {factor}, which is a symbol; \
                     arithmetic is defined over numbers"
                ),
            ));
        }
    }
    Ok(DataType::Integer)
}

fn check_types(
    program: &Program,
    rule: &FLRule,
    shape: &Shape,
    types: &HashMap<String, Vec<DataType>>,
) -> Result<()> {
    let complete: HashMap<String, Vec<Option<DataType>>> = types
        .iter()
        .map(|(name, columns)| (name.clone(), columns.iter().copied().map(Some).collect()))
        .collect();
    let vars = var_types(program, rule, shape, &complete)?;

    for atom in rule.positive_atoms().chain(rule.negated_atoms()) {
        let columns = &types[atom.name()];
        for (position, argument) in atom.arguments().iter().enumerate() {
            if let AtomArg::Const(constant) = argument {
                let column_type = columns[position];
                if literal_type(constant) != column_type {
                    return Err(refuse(
                        program,
                        rule,
                        format!(
                            "rule {rule} matches column {} of {} (a {column_type}) against the \
                             {} {constant}",
                            position + 1,
                            atom.name(),
                            literal_type(constant)
                        ),
                    )
                    .with_relation(atom.name()));
                }
            }
        }
    }

    for comparison in rule.comparisons() {
        let left = checked_expression_type(program, rule, comparison.left(), &vars)?;
        let right = checked_expression_type(program, rule, comparison.right(), &vars)?;
        if comparison.operator().is_ordered() {
            if left != DataType::Integer || right != DataType::Integer {
                return Err(refuse(
                    program,
                    rule,
                    format!(
                        "rule {rule} orders `{}` against `{}`; only numbers are ordered",
                        comparison.left(),
                        comparison.right()
                    ),
                ));
            }
        } else if left != right {
            return Err(refuse(
                program,
                rule,
                format!(
                    "rule {rule} compares `{}` (a {left}) with `{}` (a {right})",
                    comparison.left(),
                    comparison.right()
                ),
            ));
        }
    }

    for call in rule.calls() {
        let expression = call.call();
        let function = program
            .embedded_rust()
            .and_then(|embedded| embedded.function(expression.function()))
            .expect("checked by check_shape");
        for (index, (argument, parameter)) in expression
            .arguments()
            .iter()
            .zip(function.parameters().iter())
            .enumerate()
        {
            let argument_type = match argument {
                AtomArg::Var(variable) => vars.get(variable).copied().unwrap_or(DataType::Integer),
                AtomArg::Const(constant) => literal_type(constant),
                AtomArg::Placeholder => unreachable!("checked by check_shape"),
            };
            if argument_type != *parameter {
                return Err(refuse(
                    program,
                    rule,
                    format!(
                        "rule {rule} passes {argument} (a {argument_type}) to {:?}, whose \
                         parameter {} is a {parameter}: `{}`",
                        function.name(),
                        index + 1,
                        function.signature()
                    ),
                )
                .with_function(function.name()));
            }
        }
    }

    let head = rule.head();
    let columns = &types[head.name().as_str()];
    for (position, argument) in head.head_arguments().iter().enumerate() {
        let derived_type = match argument {
            HeadArg::Var(variable) => vars.get(variable).copied().unwrap_or(DataType::Integer),
            HeadArg::Arith(arithmetic) => checked_expression_type(program, rule, arithmetic, &vars)?,
            HeadArg::Aggregation(aggregation) => {
                let inner = checked_expression_type(program, rule, aggregation.arithmetic(), &vars)?;
                match aggregation.operator() {
                    AggregationOperator::Count => DataType::Integer,
                    operator => {
                        if inner != DataType::Integer {
                            return Err(refuse(
                                program,
                                rule,
                                format!(
                                    "rule {rule} aggregates `{operator}` over a symbol; only \
                                     numbers are summed or ordered"
                                ),
                            ));
                        }
                        DataType::Integer
                    }
                }
            }
        };
        let column_type = columns[position];
        if derived_type != column_type {
            return Err(refuse(
                program,
                rule,
                format!(
                    "rule {rule} derives column {} of {} as a {derived_type}, but that column \
                     is a {column_type}",
                    position + 1,
                    head.name()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::decl::DataType;
    use crate::diagnostic::Diagnostic;
    use crate::parser::Program;

    fn validate(source: &str) -> Result<Program, Diagnostic> {
        Program::parse(source, "validate-test.dl")
    }

    fn refusal(source: &str) -> String {
        let error = validate(source).expect_err("the program should be refused");
        error.to_string()
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
        )
        .unwrap();
    }

    #[test]
    fn accepts_symbol_columns_and_infers_them() {
        let program = validate(
            ".in\n\
             .decl E(k: number, v: symbol)\n\
             .printsize\n\
             .decl R(v: string, k: number)\n\
             .rule\n\
             H(v, k) :- E(k, v).\n\
             R(v, k) :- H(v, k), v = \"main\".",
        )
        .unwrap();
        assert_eq!(program.column_types("H"), Some(&[DataType::Symbol, DataType::Integer][..]));
    }

    #[test]
    fn accepts_head_constants_arithmetic_and_aggregated_expressions() {
        validate(
            ".in\n\
             .decl E(k: number, v: number)\n\
             .printsize\n\
             .decl R(k: number, v: number)\n\
             .decl S(s: number)\n\
             .decl T(k: number, s: number)\n\
             .decl U(s: number, k: number)\n\
             .rule\n\
             R(k, 7) :- E(k, v).\n\
             S(k + v) :- E(k, v).\n\
             T(k, sum(v + 100)) :- E(k, v).\n\
             U(count(v), k) :- E(k, v).",
        )
        .unwrap();
    }

    #[test]
    fn accepts_an_arity_zero_relation() {
        validate(
            ".in\n\
             .decl E(k: number)\n\
             .printsize\n\
             .decl Any()\n\
             .rule\n\
             Any() :- E(k).",
        )
        .unwrap();
    }

    #[test]
    fn refuses_a_symbol_in_arithmetic() {
        let text = refusal(
            ".in\n\
             .decl E(k: number, v: symbol)\n\
             .printsize\n\
             .decl R(k: number)\n\
             .rule\n\
             R(k + v) :- E(k, v).",
        );
        assert!(text.contains("arithmetic is defined over numbers"), "{text}");
    }

    #[test]
    fn refuses_ordering_symbols() {
        let text = refusal(
            ".in\n\
             .decl E(k: number, v: symbol)\n\
             .printsize\n\
             .decl R(k: number)\n\
             .rule\n\
             R(k) :- E(k, v), v < \"z\".",
        );
        assert!(text.contains("only numbers are ordered"), "{text}");
    }

    #[test]
    fn refuses_a_variable_typed_two_ways() {
        let text = refusal(
            ".in\n\
             .decl E(k: number)\n\
             .decl F(s: symbol)\n\
             .printsize\n\
             .decl R(k: number)\n\
             .rule\n\
             R(k) :- E(k), F(k).",
        );
        assert!(text.contains("is a number in E(k) and a symbol in F(k)"), "{text}");
    }

    #[test]
    fn refuses_a_text_constant_in_a_number_column() {
        let text = refusal(
            ".in\n\
             .decl E(k: number)\n\
             .printsize\n\
             .decl R(k: number)\n\
             .rule\n\
             R(k) :- E(k), E(\"x\").",
        );
        assert!(text.contains("against the symbol \"x\""), "{text}");
    }

    #[test]
    fn refuses_a_head_arity_that_disagrees_with_the_declaration() {
        let text = refusal(
            ".in\n\
             .decl E(k: number, v: number)\n\
             .printsize\n\
             .decl R(a: number, b: number, c: number)\n\
             .rule\n\
             R(k, v) :- E(k, v).",
        );
        assert!(text.contains("is declared with arity 3"), "{text}");
    }

    #[test]
    fn refuses_two_aggregation_operators_on_one_relation() {
        let text = refusal(
            ".in\n\
             .decl E(k: number, v: number)\n\
             .printsize\n\
             .decl R(k: number, a: number)\n\
             .rule\n\
             R(k, sum(v)) :- E(k, v).\n\
             R(k, max(v)) :- E(k, v).",
        );
        assert!(text.contains("different aggregation operators"), "{text}");
    }

    #[test]
    fn refuses_a_plain_rule_beside_an_aggregate_rule() {
        let text = refusal(
            ".in\n\
             .decl E(k: number, v: number)\n\
             .printsize\n\
             .decl R(k: number, a: number)\n\
             .rule\n\
             R(k, v) :- E(k, v).\n\
             R(k, sum(v)) :- E(k, v).",
        );
        assert!(text.contains("both a plain rule and an aggregate rule"), "{text}");
    }

    #[test]
    fn refuses_an_unsafe_negation_and_an_unbound_head() {
        let text = refusal(
            ".in\n\
             .decl E(k: number)\n\
             .printsize\n\
             .decl R(k: number)\n\
             .rule\n\
             R(k) :- E(k), !E(j).",
        );
        assert!(text.contains("unsafe var"), "{text}");
        let text = refusal(
            ".in\n\
             .decl E(k: number)\n\
             .printsize\n\
             .decl R(k: number)\n\
             .rule\n\
             R(j) :- E(k).",
        );
        assert!(text.contains("unbound head variable"), "{text}");
    }

    #[test]
    fn refuses_a_rule_with_no_positive_atom() {
        let text = refusal(
            ".in\n\
             .decl E(k: number)\n\
             .printsize\n\
             .decl R(k: number)\n\
             .rule\n\
             R(1) :- 1 = 1.",
        );
        assert!(text.contains("no positive relational atom"), "{text}");
    }

    #[test]
    fn refuses_a_call_over_the_wrong_type() {
        let text = refusal(
            ".code rust\n\
             pub fn twice(x: i64) -> i64 { x * 2 }\n\
             .endcode\n\
             .in\n\
             .decl E(s: symbol)\n\
             .printsize\n\
             .decl R(k: number)\n\
             .rule\n\
             R(y) :- E(s), y = @call(twice, s).",
        );
        assert!(text.contains("parameter 1 is a number"), "{text}");
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
        )
        .unwrap();
    }

    #[test]
    fn a_refusal_names_the_rule_and_its_line() {
        let error = validate(
            ".in\n\
             .decl E(k: number)\n\
             .printsize\n\
             .decl R(k: number)\n\
             .rule\n\
             R(k) :- E(k).\n\
             R(j) :- E(k).",
        )
        .unwrap_err();
        assert_eq!(error.location.as_ref().unwrap().line, 7);
        assert_eq!(error.rule.as_deref(), Some("R(j) :- E(k)."));
        assert_eq!(error.relations, vec!["R".to_string()]);
    }
}

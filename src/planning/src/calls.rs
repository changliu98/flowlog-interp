//! The row program: everything a rule computes after its joins.
//!
//! A rule's relational part - the joins, semijoins, antijoins and the
//! comparisons over relational variables - produces one row per body
//! solution, holding the variables the rest of the rule reads. What remains
//! is row-local: embedded calls, comparisons over call results, arithmetic in
//! the head, constants in the head, and the expression under an aggregate.
//! All of that is one *row program*: a straight-line sequence of steps over
//! the row's values, each producing a result or filtering the row, followed
//! by the head projection, which names an input, a result or a constant for
//! every head column.
//!
//! A rule that needs none of this has no row program and its relational plan
//! projects the head directly, as before. A rule that needs any of it gets
//! one, so head constants, head arithmetic, computed comparisons and
//! aggregated expressions share one mechanism with calls instead of each
//! being a rewrite the caller has to perform.

use std::collections::{HashMap, HashSet};
use std::fmt;

use catalog::rule::Catalog;
use parsing::arithmetic::{Arithmetic, ArithmeticOperator, Factor};
use parsing::compare::ComparisonOperator;
use parsing::embedded::EmbeddedRust;
use parsing::head::HeadArg;
use parsing::rule::{AtomArg, CallPredicate};
use parsing::Val;

/// One scalar a step reads: a column of the input row, the result of an
/// earlier step, or a constant of the program.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum ValueRef {
    Input(usize),
    Result(usize),
    Constant(Val),
}

/// One step of a row program, in evaluation order.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum Step {
    /// An embedded call. `output` is the result the call binds; `None` for a
    /// bool-returning filter, which drops the row when it returns false.
    Call {
        function: String,
        arguments: Vec<ValueRef>,
        output: Option<usize>,
    },
    /// Left-to-right arithmetic over values, binding a result.
    Arithmetic {
        init: ValueRef,
        rest: Vec<(ArithmeticOperator, ValueRef)>,
        output: usize,
    },
    /// A comparison of two values, dropping the row when it does not hold.
    Compare {
        left: ValueRef,
        operator: ComparisonOperator,
        right: ValueRef,
    },
}

/// The row-local part of one rule.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct RowProgram {
    input_variables: Vec<String>,
    steps: Vec<Step>,
    head: Vec<ValueRef>,
    results: usize,
    rule: String,
    line: usize,
}

impl RowProgram {
    /// The row program of a rule, or `None` when its head is a projection of
    /// relational variables and its body has no call.
    pub fn from_catalog(catalog: &Catalog, embedded: Option<&EmbeddedRust>) -> Option<Self> {
        let head_computes = catalog.head_arguments().iter().any(|argument| match argument {
            HeadArg::Var(_) => false,
            HeadArg::Arith(_) => true,
            HeadArg::Aggregation(aggregation) => !aggregation.arithmetic().is_var(),
        });
        if catalog.call_predicates().is_empty()
            && catalog.computed_comparisons().is_empty()
            && !head_computes
        {
            return None;
        }

        let positive_ids = (0..catalog.atom_names().len()).collect::<Vec<_>>();
        let positive_variables = catalog
            .vars_set(&positive_ids)
            .into_iter()
            .cloned()
            .collect::<HashSet<_>>();

        let mut builder = Builder {
            input_variables: Vec::new(),
            input_indices: HashMap::new(),
            result_indices: HashMap::new(),
            results: 0,
            steps: Vec::new(),
            positive_variables,
            rule: catalog.rule().to_string(),
        };

        for predicate in catalog.call_predicates() {
            let call = predicate.call();
            if let Some(embedded) = embedded {
                let function = embedded.function(call.function()).unwrap_or_else(|| {
                    panic!(
                        "rule {} calls unknown embedded Rust function {:?} (validation should \
                         have refused it)",
                        builder.rule,
                        call.function()
                    )
                });
                assert_eq!(
                    function.arity(),
                    call.arguments().len(),
                    "rule {} calls {:?} with the wrong arity (validation should have refused it)",
                    builder.rule,
                    call.function()
                );
            } else {
                panic!(
                    "rule {} uses @call but the program has no .code rust section (validation \
                     should have refused it)",
                    builder.rule
                );
            }
            let arguments = call
                .arguments()
                .iter()
                .map(|argument| builder.atom_argument(argument))
                .collect::<Vec<_>>();
            let output = match predicate {
                CallPredicate::Bind { output, .. } => Some(builder.bind(output)),
                CallPredicate::Filter(_) => None,
            };
            builder.steps.push(Step::Call {
                function: call.function().to_string(),
                arguments,
                output,
            });
        }

        for comparison in catalog.computed_comparisons() {
            let left = builder.expression(comparison.left());
            let right = builder.expression(comparison.right());
            builder.steps.push(Step::Compare {
                left,
                operator: comparison.operator().clone(),
                right,
            });
        }

        let head = catalog
            .head_arguments()
            .iter()
            .map(|argument| match argument {
                HeadArg::Var(variable) => builder.variable(variable),
                HeadArg::Arith(arithmetic) => builder.expression(arithmetic),
                HeadArg::Aggregation(aggregation) => builder.expression(aggregation.arithmetic()),
            })
            .collect::<Vec<_>>();

        // The relational plan projects the program's inputs; a program that
        // reads none still needs one column to be driven by, so it borrows a
        // variable the body binds. A body with no variable at all drives the
        // program by existence: its projection is the 0-column image.
        if builder.input_variables.is_empty() {
            if let Some(variable) = builder.positive_variables.iter().min().cloned() {
                builder.input(&variable);
            }
        }

        Some(Self {
            input_variables: builder.input_variables,
            steps: builder.steps,
            head,
            results: builder.results,
            rule: catalog.rule().to_string(),
            line: catalog.rule().line(),
        })
    }

    /// The relational variables the program reads, in input-column order.
    pub fn input_variables(&self) -> &[String] {
        &self.input_variables
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub fn head(&self) -> &[ValueRef] {
        &self.head
    }

    /// How many results the steps bind.
    pub fn results(&self) -> usize {
        self.results
    }

    /// The rule the program belongs to, as written, and its line.
    pub fn rule(&self) -> &str {
        &self.rule
    }

    pub fn line(&self) -> usize {
        self.line
    }

    /// The embedded functions the program calls, each once, in call order.
    pub fn functions(&self) -> Vec<&str> {
        let mut names = Vec::new();
        for step in &self.steps {
            if let Step::Call { function, .. } = step {
                if !names.contains(&function.as_str()) {
                    names.push(function.as_str());
                }
            }
        }
        names
    }
}

struct Builder {
    input_variables: Vec<String>,
    input_indices: HashMap<String, usize>,
    result_indices: HashMap<String, usize>,
    results: usize,
    steps: Vec<Step>,
    positive_variables: HashSet<String>,
    rule: String,
}

impl Builder {
    fn input(&mut self, variable: &str) -> ValueRef {
        let next = self.input_variables.len();
        let index = *self
            .input_indices
            .entry(variable.to_string())
            .or_insert_with(|| {
                self.input_variables.push(variable.to_string());
                next
            });
        ValueRef::Input(index)
    }

    fn variable(&mut self, variable: &str) -> ValueRef {
        if let Some(index) = self.result_indices.get(variable) {
            ValueRef::Result(*index)
        } else if self.positive_variables.contains(variable) {
            self.input(variable)
        } else {
            panic!(
                "rule {} reads variable {variable:?} that nothing binds (validation should have \
                 refused it)",
                self.rule
            )
        }
    }

    fn bind(&mut self, output: &str) -> usize {
        assert!(
            !self.positive_variables.contains(output) && !self.result_indices.contains_key(output),
            "rule {} binds {output:?} twice (validation should have refused it)",
            self.rule
        );
        let index = self.results;
        self.results += 1;
        self.result_indices.insert(output.to_string(), index);
        index
    }

    fn atom_argument(&mut self, argument: &AtomArg) -> ValueRef {
        match argument {
            AtomArg::Var(variable) => self.variable(variable),
            AtomArg::Const(constant) => ValueRef::Constant(constant.integer()),
            AtomArg::Placeholder => panic!(
                "rule {} passes '_' to a call (validation should have refused it)",
                self.rule
            ),
        }
    }

    fn factor(&mut self, factor: &Factor) -> ValueRef {
        match factor {
            Factor::Var(variable) => self.variable(variable),
            Factor::Const(constant) => ValueRef::Constant(constant.integer()),
        }
    }

    fn expression(&mut self, arithmetic: &Arithmetic) -> ValueRef {
        if arithmetic.is_single() {
            return self.factor(arithmetic.init());
        }
        let init = self.factor(arithmetic.init());
        let rest = arithmetic
            .rest()
            .iter()
            .map(|(operator, factor)| (operator.clone(), self.factor(factor)))
            .collect::<Vec<_>>();
        let output = self.results;
        self.results += 1;
        self.steps.push(Step::Arithmetic { init, rest, output });
        ValueRef::Result(output)
    }
}

impl fmt::Display for ValueRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValueRef::Input(index) => write!(f, "i{index}"),
            ValueRef::Result(index) => write!(f, "r{index}"),
            ValueRef::Constant(value) => write!(f, "c{value}"),
        }
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Call {
                function,
                arguments,
                output,
            } => {
                write!(
                    f,
                    "{function}({})->{}",
                    arguments
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(","),
                    output
                        .map(|index| format!("r{index}"))
                        .unwrap_or_else(|| "bool".to_string())
                )
            }
            Step::Arithmetic { init, rest, output } => {
                write!(f, "{init}")?;
                for (operator, value) in rest {
                    write!(f, "{operator}{value}")?;
                }
                write!(f, "->r{output}")
            }
            Step::Compare {
                left,
                operator,
                right,
            } => write!(f, "{left}{operator}{right}"),
        }
    }
}

impl fmt::Display for RowProgram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let steps = self
            .steps
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(";");
        let head = self
            .head
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        write!(
            f,
            "@[in={};steps={steps};head={head}]",
            self.input_variables.join(",")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{RowProgram, Step, ValueRef};
    use catalog::rule::Catalog;
    use parsing::parser::Program;

    fn program(source: &str) -> Option<RowProgram> {
        let program = Program::from_source(source, "row-program-test.dl");
        let catalog = Catalog::from_strata(&program.rules()[0]);
        RowProgram::from_catalog(&catalog, program.embedded_rust())
    }

    #[test]
    fn lowers_relational_constants_chains_filters_and_head_fields() {
        let projection = program(
            r#".code rust
pub fn add(x: i64, y: i64) -> i64 { x + y }
pub fn keep(x: i64) -> bool { x > 0 }
.endcode
.in
.decl Input(x: number)
.printsize
.decl Result(x: number, z: number)
.rule
Result(X, Z) :- Input(X), Y = @call(add, X, 1), Z = @call(add, Y, 2), @call(keep, Z).
"#,
        )
        .unwrap();

        assert_eq!(projection.input_variables(), &["X"]);
        assert_eq!(projection.steps().len(), 3);
        match &projection.steps()[0] {
            Step::Call { arguments, output, .. } => {
                assert_eq!(arguments, &[ValueRef::Input(0), ValueRef::Constant(1)]);
                assert_eq!(*output, Some(0));
            }
            step => panic!("unexpected step {step:?}"),
        }
        assert_eq!(projection.head(), &[ValueRef::Input(0), ValueRef::Result(1)]);
        assert_eq!(projection.functions(), vec!["add", "keep"]);
    }

    #[test]
    fn a_plain_projection_has_no_row_program() {
        assert!(program(
            ".in\n.decl Input(x: number, y: number)\n.printsize\n.decl Result(x: number)\n.rule\nResult(X) :- Input(X, Y), X < Y.\n"
        )
        .is_none());
    }

    #[test]
    fn head_constants_arithmetic_and_computed_comparisons_are_steps() {
        let projection = program(
            r#".code rust
pub fn twice(x: i64) -> i64 { x * 2 }
.endcode
.in
.decl Input(x: number, y: number)
.printsize
.decl Result(a: number, b: number, c: number)
.rule
Result(X + Y, 7, D) :- Input(X, Y), D = @call(twice, X), D > Y + 1.
"#,
        )
        .unwrap();
        assert_eq!(projection.input_variables(), &["X", "Y"]);
        // call, arithmetic for Y + 1, compare, arithmetic for X + Y
        assert_eq!(projection.steps().len(), 4);
        assert!(matches!(projection.steps()[2], Step::Compare { .. }));
        assert_eq!(projection.head()[1], ValueRef::Constant(7));
        assert_eq!(projection.head()[2], ValueRef::Result(0));
    }

    #[test]
    fn a_program_that_reads_nothing_borrows_a_driver() {
        let projection = program(
            ".in\n.decl Input(x: number)\n.printsize\n.decl Result(k: number)\n.rule\nResult(1) :- Input(X).\n",
        )
        .unwrap();
        assert_eq!(projection.input_variables(), &["X"]);
        assert_eq!(projection.head(), &[ValueRef::Constant(1)]);
    }
}

use std::collections::{HashMap, HashSet};
use std::fmt;

use catalog::rule::Catalog;
use parsing::embedded::{EmbeddedRust, RustReturnType};
use parsing::head::HeadArg;
use parsing::rule::{AtomArg, CallPredicate, Const};

/// One scalar consumed by an embedded call.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum CallValueRef {
    Input(usize),
    Result(usize),
    Constant(i32),
}

/// One call in textual body order. Bindings append an `i32` result to the
/// per-row result vector; filters return `bool` and keep or discard the row.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct CallStep {
    function: String,
    arguments: Vec<CallValueRef>,
    output: Option<usize>,
}

impl CallStep {
    pub fn function(&self) -> &str {
        &self.function
    }

    pub fn arguments(&self) -> &[CallValueRef] {
        &self.arguments
    }

    pub fn output(&self) -> Option<usize> {
        self.output
    }
}

/// One field emitted into the rule head after all calls have run.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum CallHeadRef {
    Input(usize),
    Result(usize),
}

/// A statically validated, row-local call program attached to a rule root.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct CallProjection {
    input_variables: Vec<String>,
    steps: Vec<CallStep>,
    head: Vec<CallHeadRef>,
}

impl CallProjection {
    pub fn from_catalog(catalog: &Catalog, embedded: Option<&EmbeddedRust>) -> Option<Self> {
        if catalog.call_predicates().is_empty() {
            return None;
        }
        let embedded = embedded.unwrap_or_else(|| {
            panic!(
                "rule {} uses @call but the program has no .code rust section",
                catalog.rule()
            )
        });

        let positive_ids = (0..catalog.atom_names().len()).collect::<Vec<_>>();
        let positive_variables = catalog
            .vars_set(&positive_ids)
            .into_iter()
            .cloned()
            .collect::<HashSet<_>>();
        let call_outputs = catalog
            .call_predicates()
            .iter()
            .filter_map(CallPredicate::output)
            .map(str::to_string)
            .collect::<HashSet<_>>();

        for comparison in catalog.comparison_predicates() {
            if let Some(variable) = comparison
                .vars_set()
                .into_iter()
                .find(|variable| call_outputs.contains(*variable))
            {
                panic!(
                    "rule {} compares call-bound variable {variable:?}; put that test in a bool-returning @call",
                    catalog.rule()
                );
            }
        }

        let mut input_variables = Vec::new();
        let mut input_indices = HashMap::new();
        let mut result_indices = HashMap::new();
        let mut steps = Vec::new();

        let input_ref = |variable: &str,
                         input_variables: &mut Vec<String>,
                         input_indices: &mut HashMap<String, usize>|
         -> CallValueRef {
            let next = input_variables.len();
            let index = *input_indices
                .entry(variable.to_string())
                .or_insert_with(|| {
                    input_variables.push(variable.to_string());
                    next
                });
            CallValueRef::Input(index)
        };

        for predicate in catalog.call_predicates() {
            let call = predicate.call();
            let function = embedded.function(call.function()).unwrap_or_else(|| {
                panic!(
                    "rule {} calls unknown embedded Rust function {:?}",
                    catalog.rule(),
                    call.function()
                )
            });
            if function.arity() != call.arguments().len() {
                panic!(
                    "rule {} calls {:?} with {} arguments, but its Rust signature takes {}",
                    catalog.rule(),
                    call.function(),
                    call.arguments().len(),
                    function.arity()
                );
            }

            let arguments = call
                .arguments()
                .iter()
                .map(|argument| match argument {
                    AtomArg::Var(variable) => {
                        if let Some(index) = result_indices.get(variable) {
                            CallValueRef::Result(*index)
                        } else if positive_variables.contains(variable) {
                            input_ref(variable, &mut input_variables, &mut input_indices)
                        } else {
                            panic!(
                                "rule {} calls {:?} before variable {variable:?} is bound by a positive atom or earlier @call",
                                catalog.rule(),
                                call.function()
                            );
                        }
                    }
                    AtomArg::Const(Const::Integer(value)) => CallValueRef::Constant(*value),
                    AtomArg::Const(Const::Text(_)) => panic!(
                        "rule {} passes text to {:?}; embedded calls currently accept only i32",
                        catalog.rule(),
                        call.function()
                    ),
                    AtomArg::Placeholder => panic!(
                        "rule {} passes '_' to {:?}; every @call input must be bound",
                        catalog.rule(),
                        call.function()
                    ),
                })
                .collect::<Vec<_>>();

            let output = match predicate {
                CallPredicate::Bind { output, .. } => {
                    if function.return_type() != RustReturnType::I32 {
                        panic!(
                            "rule {} binds bool-returning {:?}; use it as a bare @call filter",
                            catalog.rule(),
                            call.function()
                        );
                    }
                    if positive_variables.contains(output) || result_indices.contains_key(output) {
                        panic!(
                            "rule {} binds @call output {output:?} more than once or over a relational variable",
                            catalog.rule()
                        );
                    }
                    let index = result_indices.len();
                    result_indices.insert(output.clone(), index);
                    Some(index)
                }
                CallPredicate::Filter(_) => {
                    if function.return_type() != RustReturnType::Bool {
                        panic!(
                            "rule {} uses i32-returning {:?} as a filter; bind its result with 'Variable = @call(...)'",
                            catalog.rule(),
                            call.function()
                        );
                    }
                    None
                }
            };

            steps.push(CallStep {
                function: call.function().to_string(),
                arguments,
                output,
            });
        }

        let head = catalog
            .head_arguments()
            .iter()
            .map(|argument| match argument {
                HeadArg::Var(variable) => {
                    if let Some(index) = result_indices.get(variable) {
                        CallHeadRef::Result(*index)
                    } else if positive_variables.contains(variable) {
                        match input_ref(variable, &mut input_variables, &mut input_indices) {
                            CallValueRef::Input(index) => CallHeadRef::Input(index),
                            _ => unreachable!(),
                        }
                    } else {
                        panic!(
                            "rule {} emits unbound head variable {variable:?}",
                            catalog.rule()
                        );
                    }
                }
                _ => panic!(
                    "rule {} combines @call with an arithmetic or aggregate head; bind the complete value in Rust first",
                    catalog.rule()
                ),
            })
            .collect::<Vec<_>>();

        if input_variables.is_empty() {
            panic!(
                "rule {} has no relational value to drive @call; constant-only calls are not yet supported",
                catalog.rule()
            );
        }

        Some(Self {
            input_variables,
            steps,
            head,
        })
    }

    pub fn input_variables(&self) -> &[String] {
        &self.input_variables
    }

    pub fn steps(&self) -> &[CallStep] {
        &self.steps
    }

    pub fn head(&self) -> &[CallHeadRef] {
        &self.head
    }
}

impl fmt::Display for CallProjection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value_ref = |value: &CallValueRef| match value {
            CallValueRef::Input(index) => format!("i{index}"),
            CallValueRef::Result(index) => format!("r{index}"),
            CallValueRef::Constant(value) => format!("c{value}"),
        };
        let steps = self
            .steps
            .iter()
            .map(|step| {
                format!(
                    "{}({})->{}",
                    step.function,
                    step.arguments
                        .iter()
                        .map(&value_ref)
                        .collect::<Vec<_>>()
                        .join(","),
                    step.output
                        .map(|index| format!("r{index}"))
                        .unwrap_or_else(|| "bool".to_string())
                )
            })
            .collect::<Vec<_>>()
            .join(";");
        let head = self
            .head
            .iter()
            .map(|field| match field {
                CallHeadRef::Input(index) => format!("i{index}"),
                CallHeadRef::Result(index) => format!("r{index}"),
            })
            .collect::<Vec<_>>()
            .join(",");
        write!(
            f,
            "@call[in={};steps={steps};head={head}]",
            self.input_variables.join(",")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{CallHeadRef, CallProjection, CallValueRef};
    use catalog::rule::Catalog;
    use parsing::parser::Program;

    fn projection(source: &str) -> CallProjection {
        let program = Program::from_source(source, "call-plan-test.dl");
        let catalog = Catalog::from_strata(&program.rules()[0]);
        CallProjection::from_catalog(&catalog, program.embedded_rust()).unwrap()
    }

    #[test]
    fn lowers_relational_constants_chains_filters_and_head_fields() {
        let projection = projection(
            r#".code rust
pub fn add(x: i32, y: i32) -> i32 { x + y }
pub fn keep(x: i32) -> bool { x > 0 }
.endcode
.in
.decl Input(x: number)
.printsize
.decl Result(x: number, z: number)
.rule
Result(X, Z) :- Input(X), Y = @call(add, X, 1), Z = @call(add, Y, 2), @call(keep, Z).
"#,
        );

        assert_eq!(projection.input_variables(), &["X"]);
        assert_eq!(projection.steps().len(), 3);
        assert_eq!(
            projection.steps()[0].arguments(),
            &[CallValueRef::Input(0), CallValueRef::Constant(1)]
        );
        assert_eq!(
            projection.steps()[1].arguments(),
            &[CallValueRef::Result(0), CallValueRef::Constant(2)]
        );
        assert_eq!(
            projection.head(),
            &[CallHeadRef::Input(0), CallHeadRef::Result(1)]
        );
    }

    #[test]
    #[should_panic(expected = "binds bool-returning")]
    fn rejects_binding_a_boolean_export() {
        projection(
            r#".code rust
pub fn keep(x: i32) -> bool { x > 0 }
.endcode
.in
.decl Input(x: number)
.printsize
.decl Result(x: number)
.rule
Result(Y) :- Input(X), Y = @call(keep, X).
"#,
        );
    }

    #[test]
    #[should_panic(expected = "uses i32-returning")]
    fn rejects_using_a_numeric_export_as_a_filter() {
        projection(
            r#".code rust
pub fn identity(x: i32) -> i32 { x }
.endcode
.in
.decl Input(x: number)
.printsize
.decl Result(x: number)
.rule
Result(X) :- Input(X), @call(identity, X).
"#,
        );
    }
}

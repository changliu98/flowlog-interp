use planning::constraints::BaseConstraints;
use std::sync::Arc;
use arrayvec::ArrayVec;
use parsing::arithmetic::ArithmeticOperator;
use parsing::compare::ComparisonOperator;
use parsing::diagnostic::{Diagnostic, Location, Result};
use planning::arguments::TransformationArgument;
use planning::calls::{RowProgram, Step, ValueRef};
use planning::flow::TransformationFlow;
use reading::row::Row;
use reading::row::FatRow;
use reading::row::Array;
use reading::Val;
use crate::accounting::Budget;
use crate::compare::*;
use crate::native_calls::{LoadedFunction, NativeCallModule};
use planning::compare::ComparisonExprArgument;


fn const_eq_deconstructor(constraints: &BaseConstraints) -> Vec<(usize, Val)> {
    constraints.constant_eq_constraints().iter().filter_map(|(arg, constant)| match arg {
        TransformationArgument::KV((true, id)) => Some((*id, constant.integer())),
        _ => None,
    }).collect::<Vec<_>>()
}

fn var_eq_deconstructor(constraints: &BaseConstraints) -> Vec<(usize, usize)> {
    constraints.variable_eq_constraints().iter().filter_map(|(left, right)| match (left, right) {
        (TransformationArgument::KV((true, lid)), TransformationArgument::KV((true, rid))) => Some((*lid, *rid)),
        _ => None,
    }).collect::<Vec<_>>()
}




/* ------------------------------------------------------------------------ */
/* renders for map from row to row */
/* ------------------------------------------------------------------------ */
fn map_deconstructor<const N: usize>(args: &Arc<Vec<TransformationArgument>>) -> ArrayVec<usize, N> {
    args.iter().filter_map(|arg| match arg {
        TransformationArgument::KV((true, id)) => Some(*id),
        _ => None,
    }).collect::<ArrayVec<_, N>>()
}

#[inline(always)]
fn is_filtered<const M: usize>(v: &Row<M>, const_eqs: &[(usize, Val)], var_eqs: &[(usize, usize)], compares: &Vec<ComparisonExprArgument>, budget: &Budget) -> bool {
    const_eqs.iter().all(|(i, constant)| v.column(*i) == *constant) &&
    var_eqs.iter().all(|(i, j)| v.column(*i) == v.column(*j)) &&
    compares.iter().all(|compare| compare_row(v, compare, budget))
}

pub fn row_row<const M: usize, const N: usize>(flow: &TransformationFlow, budget: &Arc<Budget>) -> impl FnMut(Row<M>) -> Option<Row<N>> {
    // for the single atom rule
    // assert!(!flow.is_constrainted());
    let k_or_v_ids = if let TransformationFlow::KVToKV { key, value, .. } = flow {
        assert!(key.is_empty() || value.is_empty());
        map_deconstructor::<N>(if key.is_empty() { value } else { key })
    } else {
        panic!("row_row: must be kv flow arguments");
    };

    assert_eq!(k_or_v_ids.len(), N, "vids arity ≠ row stack arity");

    let constraints = flow.constraints();
    let const_eqs = const_eq_deconstructor(constraints);
    let var_eqs = var_eq_deconstructor(constraints);
    let compares = flow.compares().clone();
    let budget = Arc::clone(budget);

    #[inline(always)]
    move |v|
    if !budget.stopped() && is_filtered(&v, &const_eqs, &var_eqs, &compares, &budget) {
        let mut row = Row::<N>::builder();
        for id in &k_or_v_ids { row.push(v.column(*id)); }
        Some(row.finish())
    } else {
        None
    }
}


/* ------------------------------------------------------------------------ */
/* renders for map from row to kv */
/* ------------------------------------------------------------------------ */
pub fn row_kv<const M: usize, const K: usize, const V: usize>(flow: &TransformationFlow, budget: &Arc<Budget>) -> impl FnMut(Row<M>) -> Option<(Row<K>, Row<V>)> {
    // assert!(!flow.is_constrainted());
    let (kids, vids) =
        if let TransformationFlow::KVToKV { key, value, .. } = flow {
            (map_deconstructor::<K>(key), map_deconstructor::<V>(value))
        } else {
            panic!("row_kv: must be a kv flow");
        };

    assert_eq!(kids.len(), K, "kids arity ≠ row stack arity");
    assert_eq!(vids.len(), V, "vids arity ≠ row stack arity");

    let constraints = flow.constraints();
    let const_eqs = const_eq_deconstructor(constraints);
    let var_eqs = var_eq_deconstructor(constraints);
    let compares = flow.compares().clone();
    let budget = Arc::clone(budget);

    #[inline(always)]
    move |v|
    if !budget.stopped() && is_filtered(&v, &const_eqs, &var_eqs, &compares, &budget) {
        let mut key = Row::<K>::builder();
        let mut value = Row::<V>::builder();
        for id in &kids { key.push(v.column(*id)); }
        for id in &vids { value.push(v.column(*id)); }

        Some((key.finish(), value.finish()))
    } else {
        None
    }
}


/* ------------------------------------------------------------------------ */
/* the row program */
/* ------------------------------------------------------------------------ */

enum ResolvedStep {
    Call {
        name: String,
        function: LoadedFunction,
        arguments: Vec<ValueRef>,
        output: Option<usize>,
    },
    Arithmetic {
        init: ValueRef,
        rest: Vec<(ArithmeticOperator, ValueRef)>,
        output: usize,
    },
    Compare {
        left: ValueRef,
        operator: ComparisonOperator,
        right: ValueRef,
    },
}

/// One rule's row program, resolved once per dataflow: every function is a
/// pointer, every value a column index, so a row costs no lookup.
pub struct RowProgramRunner {
    steps: Vec<ResolvedStep>,
    head: Vec<ValueRef>,
    results: usize,
    module: Option<Arc<NativeCallModule>>,
    budget: Arc<Budget>,
    rule: String,
    location: Option<Location>,
}

impl RowProgramRunner {
    pub fn new(
        program: &RowProgram,
        module: Option<&Arc<NativeCallModule>>,
        budget: &Arc<Budget>,
        program_name: &str,
    ) -> Result<Self> {
        let mut steps = Vec::with_capacity(program.steps().len());
        for step in program.steps() {
            steps.push(match step {
                Step::Call {
                    function,
                    arguments,
                    output,
                } => {
                    let loaded = module
                        .and_then(|module| module.function(function))
                        .copied()
                        .ok_or_else(|| {
                            Diagnostic::internal(format!(
                                "rule {} calls embedded function {function:?}, which was \
                                 validated but not loaded",
                                program.rule()
                            ))
                            .with_function(function.clone())
                        })?;
                    ResolvedStep::Call {
                        name: function.clone(),
                        function: loaded,
                        arguments: arguments.clone(),
                        output: *output,
                    }
                }
                Step::Arithmetic { init, rest, output } => ResolvedStep::Arithmetic {
                    init: *init,
                    rest: rest.clone(),
                    output: *output,
                },
                Step::Compare {
                    left,
                    operator,
                    right,
                } => ResolvedStep::Compare {
                    left: *left,
                    operator: operator.clone(),
                    right: *right,
                },
            });
        }
        Ok(Self {
            steps,
            head: program.head().to_vec(),
            results: program.results(),
            module: module.cloned(),
            budget: Arc::clone(budget),
            rule: program.rule().to_string(),
            location: (program.line() > 0)
                .then(|| Location::new(program_name, program.line(), 0)),
        })
    }

    #[inline]
    fn value(&self, input: &dyn Array, results: &[Val], value: ValueRef) -> Val {
        match value {
            ValueRef::Input(index) => input.column(index),
            ValueRef::Result(index) => results[index],
            ValueRef::Constant(value) => value,
        }
    }

    fn fault(&self, diagnostic: Diagnostic) {
        let mut diagnostic = diagnostic.with_rule(&self.rule);
        if diagnostic.location.is_none() {
            if let Some(location) = &self.location {
                diagnostic = diagnostic.with_location(location.clone());
            }
        }
        self.budget.fault(diagnostic);
    }

    /// The head row the program derives from `input`, or `None` when a filter
    /// drops the row, the evaluation is stopping, or a step faulted (the
    /// fault is recorded on the budget).
    pub fn evaluate(&self, input: &dyn Array) -> Option<Vec<Val>> {
        self.evaluate_as(input)
    }

    fn evaluate_as<R: FromIterator<Val>>(&self, input: &dyn Array) -> Option<R> {
        if self.budget.stopped() {
            return None;
        }
        let mut results: ArrayVec<Val, 16> = ArrayVec::new();
        let mut spilled: Vec<Val> = Vec::new();
        let use_spill = self.results > results.capacity();
        if use_spill {
            spilled.resize(self.results, 0);
        } else {
            for _ in 0..self.results {
                results.push(0);
            }
        }
        let results_slice: &mut [Val] = if use_spill { &mut spilled } else { &mut results };

        for step in &self.steps {
            match step {
                ResolvedStep::Call {
                    name,
                    function,
                    arguments,
                    output,
                } => {
                    let cells = arguments
                        .iter()
                        .map(|argument| self.value(input, results_slice, *argument))
                        .collect::<ArrayVec<Val, 16>>();
                    let module = self.module.as_ref().expect("a call resolved a module");
                    let value = match module.call(name, function, &cells) {
                        Ok(value) => value,
                        Err(diagnostic) => {
                            self.fault(diagnostic);
                            return None;
                        }
                    };
                    match output {
                        Some(index) => results_slice[*index] = value,
                        None => match value {
                            0 => return None,
                            1 => {}
                            other => {
                                self.fault(Diagnostic::internal(format!(
                                    "bool-returning embedded function {name:?} produced {other}"
                                )));
                                return None;
                            }
                        },
                    }
                }
                ResolvedStep::Arithmetic { init, rest, output } => {
                    let mut value = self.value(input, results_slice, *init);
                    for (operator, operand) in rest {
                        let operand = self.value(input, results_slice, *operand);
                        value = match apply(operator, value, operand) {
                            Some(value) => value,
                            None => {
                                self.fault(Diagnostic::evaluation(format!(
                                    "division by zero: {value} {operator} {operand}"
                                )));
                                return None;
                            }
                        };
                    }
                    results_slice[*output] = value;
                }
                ResolvedStep::Compare {
                    left,
                    operator,
                    right,
                } => {
                    let left = self.value(input, results_slice, *left);
                    let right = self.value(input, results_slice, *right);
                    if !compare_ints(left, operator, right) {
                        return None;
                    }
                }
            }
        }

        Some(
            self.head
                .iter()
                .map(|value| self.value(input, results_slice, *value))
                .collect(),
        )
    }
}

/// Left-to-right integer arithmetic, wrapping on overflow; `None` on a zero
/// divisor.
#[inline]
pub fn apply(operator: &ArithmeticOperator, left: Val, right: Val) -> Option<Val> {
    match operator {
        ArithmeticOperator::Plus => Some(left.wrapping_add(right)),
        ArithmeticOperator::Minus => Some(left.wrapping_sub(right)),
        ArithmeticOperator::Multiply => Some(left.wrapping_mul(right)),
        ArithmeticOperator::Divide => {
            if right == 0 {
                None
            } else {
                Some(left.wrapping_div(right))
            }
        }
        ArithmeticOperator::Modulo => {
            if right == 0 {
                None
            } else {
                Some(left.wrapping_rem(right))
            }
        }
    }
}

pub fn row_program<const M: usize, const N: usize>(
    runner: &Arc<RowProgramRunner>,
) -> impl FnMut(Row<M>) -> Option<Row<N>> {
    let runner = Arc::clone(runner);
    move |input| runner.evaluate_as(&input)
}


/* ------------------------------------------------------------------------ */
/* Fat mode versions */
/* ------------------------------------------------------------------------ */

fn map_deconstructor_fat(args: &Arc<Vec<TransformationArgument>>) -> Vec<usize> {
    args.iter().filter_map(|arg| match arg {
        TransformationArgument::KV((true, id)) => Some(*id),
        _ => None,
    }).collect::<Vec<_>>()
}

#[inline(always)]
fn is_filtered_fat(v: &FatRow, const_eqs: &[(usize, Val)], var_eqs: &[(usize, usize)], compares: &Vec<ComparisonExprArgument>, budget: &Budget) -> bool {
    const_eqs.iter().all(|(i, constant)| v.column(*i) == *constant) &&
    var_eqs.iter().all(|(i, j)| v.column(*i) == v.column(*j)) &&
    compares.iter().all(|compare| compare_row(v, compare, budget))
}

pub fn row_row_fat(flow: &TransformationFlow, budget: &Arc<Budget>) -> impl FnMut(FatRow) -> Option<FatRow> {
    let k_or_v_ids = if let TransformationFlow::KVToKV { key, value, .. } = flow {
        assert!(key.is_empty() || value.is_empty());
        map_deconstructor_fat(if key.is_empty() { value } else { key })
    } else {
        panic!("row_row_fat: must be kv flow arguments");
    };

    let constraints = flow.constraints();
    let const_eqs = const_eq_deconstructor(constraints);
    let var_eqs = var_eq_deconstructor(constraints);
    let compares = flow.compares().clone();
    let budget = Arc::clone(budget);

    #[inline(always)]
    move |v|
    if !budget.stopped() && is_filtered_fat(&v, &const_eqs, &var_eqs, &compares, &budget) {
        let mut row = FatRow::new();
        for id in &k_or_v_ids { row.push(v.column(*id)); }
        Some(row)
    } else {
        None
    }
}

pub fn row_program_fat(runner: &Arc<RowProgramRunner>) -> impl FnMut(FatRow) -> Option<FatRow> {
    let runner = Arc::clone(runner);
    move |input| runner.evaluate_as(&input)
}

pub fn row_kv_fat(flow: &TransformationFlow, budget: &Arc<Budget>) -> impl FnMut(FatRow) -> Option<(FatRow, FatRow)> {
    let (kids, vids) =
        if let TransformationFlow::KVToKV { key, value, .. } = flow {
            (map_deconstructor_fat(key), map_deconstructor_fat(value))
        } else {
            panic!("row_kv_fat: must be a kv flow");
        };

    let constraints = flow.constraints();
    let const_eqs = const_eq_deconstructor(constraints);
    let var_eqs = var_eq_deconstructor(constraints);
    let compares = flow.compares().clone();
    let budget = Arc::clone(budget);

    #[inline(always)]
    move |v|
    if !budget.stopped() && is_filtered_fat(&v, &const_eqs, &var_eqs, &compares, &budget) {
        let mut key = FatRow::new();
        let mut value = FatRow::new();
        for id in &kids { key.push(v.column(*id)); }
        for id in &vids { value.push(v.column(*id)); }

        Some((key, value))
    } else {
        None
    }
}

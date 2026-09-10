//! Why a row is in a relation: one-step provenance over final states.
//!
//! A witness for a derived row names a rule that derives it and, for every
//! positive atom of that rule's body, the row it matched. It is found by
//! joining the rule's body over the final states, bound by the head: for an
//! ordinary rule the first body solution whose head is the row; for an
//! aggregate rule the solutions of the row's group, which are what the
//! aggregate was computed over. Negated atoms are checked, not listed:
//! their contribution is an absence.
//!
//! This is an explanation of the fixed point, not a proof tree: a parent in a
//! recursive relation is itself explained by another call. Every relation is
//! indexed lazily on the columns a lookup binds, so explaining many rows of
//! one program costs one index per distinct access pattern.

use crate::accounting::Budget;
use crate::cache::RelationState;
use crate::map::RowProgramRunner;
use crate::native_calls::NativeCallModule;
use catalog::rule::Catalog;
use parsing::aggregation::AggregationOperator;
use parsing::arithmetic::{Arithmetic, Factor};
use parsing::compare::ComparisonExpr;
use parsing::diagnostic::{Diagnostic, Result};
use parsing::head::HeadArg;
use parsing::parser::Program;
use parsing::rule::{Atom, AtomArg, FLRule};
use parsing::Val;
use planning::calls::RowProgram;
use reading::row::{Array, FatRow};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

/// The most body solutions listed for one aggregated row.
const GROUP_LIMIT: usize = 256;

/// One parent of a witnessed row: the relation and the row matched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parent {
    pub relation: String,
    pub row: Vec<Val>,
}

/// Why a row is in a relation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Witness {
    pub relation: String,
    pub row: Vec<Val>,
    /// `None` when no rule derives the row: it is an input, or it is not in
    /// the relation at all.
    pub rule: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// Whether the row is present in the relation's final state.
    pub present: bool,
    /// Whether the row is in an input relation.
    pub input: bool,
    /// Parents of the first body solution, or of every solution of an
    /// aggregate's group (bounded).
    pub parents: Vec<Parent>,
    /// For an aggregate rule: how many body solutions the group had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_size: Option<usize>,
}

type Index = HashMap<Vec<Val>, Vec<usize>>;

/// Explains rows of one program over its final states.
pub struct Explainer<'a> {
    program: &'a Program,
    states: &'a HashMap<String, Arc<RelationState>>,
    module: Option<&'a Arc<NativeCallModule>>,
    budget: &'a Arc<Budget>,
    indexes: RefCell<HashMap<(String, Vec<usize>), Arc<Index>>>,
    inputs: std::collections::HashSet<String>,
}

impl<'a> Explainer<'a> {
    pub fn new(
        program: &'a Program,
        states: &'a HashMap<String, Arc<RelationState>>,
        module: Option<&'a Arc<NativeCallModule>>,
        budget: &'a Arc<Budget>,
    ) -> Self {
        Self {
            program,
            states,
            module,
            budget,
            indexes: RefCell::new(HashMap::new()),
            inputs: program
                .edbs()
                .iter()
                .map(|declaration| declaration.name().to_string())
                .collect(),
        }
    }

    fn state(&self, relation: &str) -> Option<&Arc<RelationState>> {
        self.states.get(relation)
    }

    /// The rows of `relation` whose `columns` equal `key`, by index.
    fn index(&self, relation: &str, columns: &[usize]) -> Arc<Index> {
        let key = (relation.to_string(), columns.to_vec());
        if let Some(index) = self.indexes.borrow().get(&key) {
            return Arc::clone(index);
        }
        let mut index: Index = HashMap::new();
        if let Some(state) = self.state(relation) {
            for (position, row) in state.rows.iter().enumerate() {
                let probe = columns.iter().map(|column| row[*column]).collect::<Vec<_>>();
                index.entry(probe).or_default().push(position);
            }
        }
        let index = Arc::new(index);
        self.indexes.borrow_mut().insert(key, Arc::clone(&index));
        index
    }

    /// Explain one row.
    pub fn explain(&self, relation: &str, row: &[Val]) -> Result<Witness> {
        let present = self
            .state(relation)
            .is_some_and(|state| state.rows.binary_search_by(|candidate| candidate.as_slice().cmp(row)).is_ok());
        let mut witness = Witness {
            relation: relation.to_string(),
            row: row.to_vec(),
            rule: None,
            rule_text: None,
            line: None,
            present,
            input: self.inputs.contains(relation),
            parents: Vec::new(),
            group_size: None,
        };
        if !present {
            return Ok(witness);
        }
        for (index, rule) in self.program.rules().iter().enumerate() {
            if rule.head().name() != relation || rule.is_sideways() {
                continue;
            }
            self.budget.poll()?;
            if let Some((parents, group_size)) = self.witness_of(rule, row)? {
                witness.rule = Some(index);
                witness.rule_text = Some(rule.to_string());
                witness.line = (rule.line() > 0).then_some(rule.line());
                witness.parents = parents;
                witness.group_size = group_size;
                return Ok(witness);
            }
        }
        Ok(witness)
    }

    /// The parents of `row` under `rule`, if the rule derives it.
    fn witness_of(&self, rule: &FLRule, row: &[Val]) -> Result<Option<(Vec<Parent>, Option<usize>)>> {
        let catalog = Catalog::from_strata(rule);
        let row_program = RowProgram::from_catalog(&catalog, self.program.embedded_rust());
        let runner = match &row_program {
            Some(program) => Some(RowProgramRunner::new(
                program,
                self.module,
                self.budget,
                self.program.name(),
            )?),
            None => None,
        };
        let head = rule.head();
        let aggregate = head.aggregate_position();

        // Bind what the head fixes: a plain variable in a non-aggregate
        // column is known from the row.
        let mut bindings: HashMap<String, Val> = HashMap::new();
        for (position, argument) in head.head_arguments().iter().enumerate() {
            if Some(position) == aggregate {
                continue;
            }
            if let HeadArg::Var(variable) = argument {
                if let Some(known) = bindings.insert(variable.clone(), row[position]) {
                    if known != row[position] {
                        return Ok(None);
                    }
                }
            }
        }

        let positives = rule.positive_atoms().collect::<Vec<_>>();
        let mut solutions: Vec<(Vec<Parent>, Val)> = Vec::new();
        let mut aggregated_values: Vec<Val> = Vec::new();
        let mut group_size = 0usize;
        let aggregation = head.aggregation();

        let mut on_solution = |bindings: &HashMap<String, Val>, parents: &[Parent]| -> Result<bool> {
            // negations, relational comparisons, then the row program or the
            // plain head
            for atom in rule.negated_atoms() {
                if self.matches_any(atom, bindings) {
                    return Ok(false);
                }
            }
            for comparison in catalog.comparison_predicates() {
                if !self.comparison_holds(comparison, bindings)? {
                    return Ok(false);
                }
            }
            let derived = match (&runner, &row_program) {
                (Some(runner), Some(program)) => {
                    let mut input = FatRow::new();
                    for variable in program.input_variables() {
                        input.push(bindings[variable]);
                    }
                    match runner.evaluate(&input) {
                        Some(values) => values,
                        None => {
                            if let Some(fault) = self.budget.recorded() {
                                return Err(fault);
                            }
                            return Ok(false);
                        }
                    }
                }
                _ => head
                    .head_arguments()
                    .iter()
                    .map(|argument| match argument {
                        HeadArg::Var(variable) => bindings[variable],
                        HeadArg::Aggregation(aggregation) => match aggregation.arithmetic().init() {
                            Factor::Var(variable) => bindings[variable],
                            Factor::Const(constant) => constant.integer(),
                        },
                        HeadArg::Arith(_) => unreachable!("head arithmetic has a row program"),
                    })
                    .collect(),
            };
            match aggregate {
                None => {
                    if derived == row {
                        solutions.push((parents.to_vec(), 0));
                        return Ok(true);
                    }
                    Ok(false)
                }
                Some(position) => {
                    let same_group = derived
                        .iter()
                        .enumerate()
                        .all(|(column, value)| column == position || *value == row[column]);
                    if same_group {
                        group_size += 1;
                        if solutions.len() < GROUP_LIMIT {
                            solutions.push((parents.to_vec(), derived[position]));
                        }
                        aggregated_values.push(derived[position]);
                    }
                    Ok(false)
                }
            }
        };

        let mut parents = Vec::with_capacity(positives.len());
        self.search(&positives, 0, &mut bindings, &mut parents, &mut on_solution)?;

        match (aggregate, aggregation) {
            (None, _) => Ok(solutions.pop().map(|(parents, _)| (parents, None))),
            (Some(position), Some(aggregation)) => {
                if group_size == 0 {
                    return Ok(None);
                }
                let expected = row[position];
                let computed = match aggregation.operator() {
                    AggregationOperator::Count => Some(group_size as Val),
                    AggregationOperator::Sum => Some(
                        aggregated_values
                            .iter()
                            .fold(0 as Val, |sum, value| sum.wrapping_add(*value)),
                    ),
                    AggregationOperator::Min => aggregated_values.iter().min().copied(),
                    AggregationOperator::Max => aggregated_values.iter().max().copied(),
                };
                if computed != Some(expected) {
                    return Ok(None);
                }
                let parents = match aggregation.operator() {
                    // the solutions that achieve the extremum explain it
                    AggregationOperator::Min | AggregationOperator::Max => solutions
                        .iter()
                        .filter(|(_, value)| *value == expected)
                        .flat_map(|(parents, _)| parents.iter().cloned())
                        .collect(),
                    _ => solutions
                        .iter()
                        .flat_map(|(parents, _)| parents.iter().cloned())
                        .collect(),
                };
                Ok(Some((parents, Some(group_size))))
            }
            (Some(_), None) => unreachable!("an aggregate position has an aggregation"),
        }
    }

    /// Enumerate body solutions over the positive atoms, in order.
    fn search(
        &self,
        atoms: &[&Atom],
        depth: usize,
        bindings: &mut HashMap<String, Val>,
        parents: &mut Vec<Parent>,
        on_solution: &mut dyn FnMut(&HashMap<String, Val>, &[Parent]) -> Result<bool>,
    ) -> Result<bool> {
        if depth == atoms.len() {
            return on_solution(bindings, parents);
        }
        let atom = atoms[depth];
        let Some(state) = self.state(atom.name()) else {
            return Ok(false);
        };
        // bound columns: constants and already-bound variables
        let mut columns = Vec::new();
        let mut probe = Vec::new();
        for (position, argument) in atom.arguments().iter().enumerate() {
            match argument {
                AtomArg::Const(constant) => {
                    columns.push(position);
                    probe.push(constant.integer());
                }
                AtomArg::Var(variable) => {
                    if let Some(value) = bindings.get(variable) {
                        columns.push(position);
                        probe.push(*value);
                    }
                }
                AtomArg::Placeholder => {}
            }
        }
        let index = self.index(atom.name(), &columns);
        let Some(candidates) = index.get(&probe) else {
            return Ok(false);
        };
        for &candidate in candidates {
            if self.budget.stopped() {
                self.budget.poll()?;
            }
            let matched = &state.rows[candidate];
            // bind the free variables, checking repeated variables agree
            let mut newly_bound = Vec::new();
            let mut consistent = true;
            for (position, argument) in atom.arguments().iter().enumerate() {
                if let AtomArg::Var(variable) = argument {
                    match bindings.get(variable) {
                        Some(value) => {
                            if *value != matched[position] {
                                consistent = false;
                                break;
                            }
                        }
                        None => {
                            bindings.insert(variable.clone(), matched[position]);
                            newly_bound.push(variable.clone());
                        }
                    }
                }
            }
            let mut found = false;
            if consistent {
                parents.push(Parent {
                    relation: atom.name().to_string(),
                    row: matched.clone(),
                });
                found = self.search(atoms, depth + 1, bindings, parents, on_solution)?;
                parents.pop();
            }
            for variable in newly_bound {
                bindings.remove(&variable);
            }
            if found {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether any row of the negated atom's relation matches the bindings.
    fn matches_any(&self, atom: &Atom, bindings: &HashMap<String, Val>) -> bool {
        let mut columns = Vec::new();
        let mut probe = Vec::new();
        for (position, argument) in atom.arguments().iter().enumerate() {
            match argument {
                AtomArg::Const(constant) => {
                    columns.push(position);
                    probe.push(constant.integer());
                }
                AtomArg::Var(variable) => {
                    columns.push(position);
                    probe.push(bindings[variable]);
                }
                AtomArg::Placeholder => {}
            }
        }
        self.index(atom.name(), &columns)
            .get(&probe)
            .is_some_and(|rows| !rows.is_empty())
    }

    fn evaluate(&self, arithmetic: &Arithmetic, bindings: &HashMap<String, Val>) -> Result<Val> {
        let value = |factor: &Factor| match factor {
            Factor::Var(variable) => bindings[variable],
            Factor::Const(constant) => constant.integer(),
        };
        let mut result = value(arithmetic.init());
        for (operator, factor) in arithmetic.rest() {
            result = crate::map::apply(operator, result, value(factor)).ok_or_else(|| {
                Diagnostic::evaluation(format!("division by zero while evaluating `{arithmetic}`"))
            })?;
        }
        Ok(result)
    }

    fn comparison_holds(&self, comparison: &ComparisonExpr, bindings: &HashMap<String, Val>) -> Result<bool> {
        let left = self.evaluate(comparison.left(), bindings)?;
        let right = self.evaluate(comparison.right(), bindings)?;
        Ok(crate::compare::compare_ints(left, comparison.operator(), right))
    }
}

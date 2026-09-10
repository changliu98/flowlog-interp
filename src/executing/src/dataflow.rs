//! Assembling group plans into a differential dataflow.
//!
//! `assemble_group` turns one group's transformations into operators over a
//! scope. `Assembly` is what one evaluation asks a worker set to run: some
//! computations, each a list of groups over injected input states with the
//! relations to capture at its boundary. All the computations of an assembly
//! share one dataflow and one set of input collections, but each has its own
//! relation maps, so two computations contributing to one head cannot see
//! each other's rows.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use itertools::Itertools;
use tracing::debug;

extern crate timely;
extern crate differential_dataflow;

// local modules
use planning::strata::GroupStrataQueryPlan;
use planning::transformations::Transformation;
use planning::collections::CollectionSignature;
use crate::accounting::Budget;
use crate::cache::RelationState;
use crate::collector::non_recursive_collector;
use crate::collector::recursive_collector;
use crate::collector::inspector;
use crate::dataflow::timely::dataflow::Scope;
use crate::transformer::*;
use crate::Time;
use crate::Iter;
use crate::map::*;
use crate::native_calls::NativeCallModule;

use parsing::diagnostic::Result;
use macros::*;
use reading::rel::Rel::*;
use reading::rel::DoubleRel::*;
use reading::rel::{DoubleRel, Rel};
use reading::arrangements::{ArrangedDict, ArrangedSet};
use reading::reader::*;
use reading::inspect::*;
use catalog::head::AggregationHeadIDB;
use timely::communication::Allocate;
use timely::dataflow::operators::probe::Handle as ProbeHandle;
use timely::worker::Worker;

type RowMap<G> = HashMap<Arc<CollectionSignature>, Arc<Rel<G>>>;
type KvMap<G> = HashMap<Arc<CollectionSignature>, (Arc<DoubleRel<G>>, Arc<ArrangedDict<G>>)>;
type KMap<G> = HashMap<Arc<CollectionSignature>, (Arc<Rel<G>>, Arc<ArrangedSet<G>>)>;

/// What `assemble_group` needs beside the plan.
pub(crate) struct AssemblyContext<'a> {
    pub fat_mode: bool,
    pub native_calls: &'a Option<Arc<NativeCallModule>>,
    pub idb_map: &'a HashMap<String, AggregationHeadIDB>,
    pub budget: &'a Arc<Budget>,
    pub program_name: &'a str,
}

/// Assemble one group's operators into `scope`.
///
/// Inputs are read from the maps and the group's heads are left in `row_map`
/// when it returns.
pub(crate) fn assemble_group<G>(
    scope: &mut G,
    group_plan: &GroupStrataQueryPlan,
    row_map: &mut RowMap<G>,
    kv_map: &mut KvMap<G>,
    k_map: &mut KMap<G>,
    context: &AssemblyContext<'_>,
) -> Result<()>
where
    G: Scope<Timestamp = Time>,
{
    let fat_mode = context.fat_mode;
    let native_calls = context.native_calls;
    let idb_map = context.idb_map;
    let budget = context.budget;

    // Every row program is resolved before any operator is built, so a
    // function that failed to load is a refusal here and never a hole in a
    // half-assembled scope.
    let mut runners: HashMap<Arc<CollectionSignature>, Arc<RowProgramRunner>> = HashMap::new();
    for transformation in group_plan.strata_plan() {
        if let Transformation::Compute { program, output, .. } = transformation {
            let runner = RowProgramRunner::new(
                program,
                native_calls.as_ref(),
                budget,
                context.program_name,
            )?;
            runners.insert(Arc::clone(output.signature()), Arc::new(runner));
        }
    }

    if !group_plan.is_recursive() {
        /* construct dataflow for a non-recursive strata */
        for next_transformation in group_plan.strata_plan() {
            let output = next_transformation.output();
            let output_signature = output.signature();
            let (ok, ov) = output.arity();
            let target = ok + ov;

            if next_transformation.is_unary() {
                let unary = next_transformation.unary();
                let (ik, iv) = unary.arity();
                let input_rel = row_map.get(unary.signature()).expect(&format!("row absent for unary op: {}", unary.signature()));

                match next_transformation {
                    Transformation::Compute { .. } => {
                        assert!(ik == 0 && ok == 0);
                        let runner = Arc::clone(&runners[output_signature]);
                        let output_rel = Arc::new(codegen_row_program!());
                        row_map.insert(Arc::clone(output_signature), output_rel);
                    },

                    Transformation::RowToRow { flow, is_no_op, .. } => { // (1) single op, tc(x, y) :- arc(y, x).
                        assert!(ik == 0 && ok == 0);
                        let output_rel = if *is_no_op { Arc::clone(input_rel) } else { Arc::new(codegen_row_row!()) };
                        row_map.insert(Arc::clone(output_signature), output_rel);
                    },

                    Transformation::RowToK { flow, is_no_op, .. } => { // (2) leaf op for semijn or aj
                        assert!(ik == 0 && ov == 0);
                        let output_rel = if *is_no_op {
                            Arc::clone(input_rel)
                        } else {
                            Arc::new(codegen_row_row!().dedup())
                        };
                        k_map.insert(
                            Arc::clone(output_signature),
                            (Arc::clone(&output_rel), Arc::new(output_rel.arrange_set()))
                        );
                    },

                    Transformation::RowToKv { flow, .. } => { // (3) leaf op for jn
                        assert_eq!(ik, 0);
                        let output_kv = Arc::new(codegen_row_kv!());
                        kv_map.insert(
                            Arc::clone(output_signature),
                            (Arc::clone(&output_kv), Arc::new(output_kv.arrange_dict()))
                        );
                    },

                    _ => panic!("abnormal unary transformation"),
                }
            } else {
                let binary = next_transformation.binary();
                let (ik0, mut iv0) = binary.0.arity();
                let (ik1, mut iv1) = binary.1.arity();
                assert_eq!(ik0, ik1);

                let (large, small, flow) = if iv0 < iv1 {
                        std::mem::swap(&mut iv0, &mut iv1);
                        (binary.1.signature(), binary.0.signature(), &next_transformation.flow().jn_flip())
                    } else {
                        (binary.0.signature(), binary.1.signature(), next_transformation.flow())
                    };

                let output_rel = match next_transformation {
                        Transformation::JnKvKv { .. } =>
                            kv_jn_kv(large, small, kv_map, ik0, iv0, iv1, target, flow, budget),

                        Transformation::JnKvK { .. } | Transformation::JnKKv { .. } =>
                            kv_jn_k(large, small, kv_map, k_map, ik0, iv0, iv1, target, flow, budget),

                        Transformation::JnKK { .. } =>
                            k_jn_k(large, small, k_map, ik0, iv0, iv1, target, flow, budget),

                        Transformation::Cartesian { .. } =>
                            cartesian(large, small, row_map, iv0, iv1, target, flow, budget),

                        Transformation::NjKvK { .. } =>
                            kv_aj_k(large, small, kv_map, k_map, ik0, iv0, iv1, target, flow, budget),

                        Transformation::NjKK { .. } =>
                            k_aj_k(large, small, k_map, ik0, iv0, iv1, target, flow, budget),

                        _ => panic!("abnormal binary transformation"),
                    };

                match (ok, ov) {
                    (0, _) => { // jn → row
                        row_map.insert(Arc::clone(output_signature), Arc::clone(&output_rel));
                    },
                    (_, 0) => { // jn → k
                        k_map.insert(
                            Arc::clone(output_signature),
                            (Arc::clone(&output_rel), Arc::new(output_rel.arrange_set()))
                        );
                    }
                    _ => { // jn → kv
                        let output_kv = Arc::new(output_rel.arrange_double(ok));
                        kv_map.insert(
                            Arc::clone(output_signature),
                            (Arc::clone(&output_kv), Arc::new(output_kv.arrange_dict()))
                        );
                    }
                }
            }
        }

        /* concat idbs of the non-recursive strata into row_map */
        non_recursive_collector(
            group_plan.last_signatures_map(),
            row_map,
            idb_map,
        );

        /* inspect idbs of the non-recursive strata (optional) */
        if tracing::level_enabled!(tracing::Level::DEBUG) {
            inspector(
                &group_plan.head_signatures_set(),
                row_map,
                false
            );
        }

    } else {
        let recursive_out_map = scope.iterative::<Iter, _, _>(|scope| {
            /* (1) construct iterative variables for strata idbs */
            let head_signatures_set = group_plan.head_signatures_set().clone();
            let mut variables_map = HashMap::with_capacity(head_signatures_set.len());
            let mut variables_next_map = HashMap::with_capacity(head_signatures_set.len());

            for (head_name, head_arity) in group_plan.heads().iter().sorted_by_key(|x| x.0) {
                // A sideways slice is not an iterative variable: later rules of
                // the group read it from the nested row map.
                if group_plan.is_sideways_head(head_name) {
                    continue;
                }

                variables_map.insert(
                    Arc::new(CollectionSignature::new_atom(head_name)),
                    construct_var(scope, *head_arity, fat_mode)
                );
            }

            let mut nest_row_map = HashMap::new();
            let mut nest_kv_map = HashMap::new();
            let mut nest_k_map = HashMap::new();

            let dependent_signatures = group_plan.enter_scope_set();
            for dependent_signature in dependent_signatures.iter().sorted_by_key(|sig| sig.name()) {
                if group_plan.is_sideways_head(dependent_signature.name()) {
                    continue;
                }

                if let Some(dependent_rel) = row_map.get(dependent_signature) { // rel has been created prior to the strata
                    if head_signatures_set.contains(dependent_signature) {
                        // (1) rel from prior strata will be part of the eventual idb
                        variables_next_map.insert(
                            Arc::clone(dependent_signature),
                            Arc::new(dependent_rel.enter(scope))
                        );
                    } else {
                        // (2) rel from prior strata purely for joins
                        nest_row_map.insert(
                            Arc::clone(dependent_signature),
                            Arc::new(dependent_rel.enter(scope))
                        );
                    }
                } else if let Some((dependent_kv, _)) = kv_map.get(dependent_signature) {
                    // (3) dict from prior strata purely for joins
                    let nested_kv = Arc::new(dependent_kv.enter(scope));
                    let nested_dict = Arc::new(nested_kv.arrange_dict());
                    nest_kv_map.insert(
                        Arc::clone(dependent_signature),
                        (nested_kv, nested_dict)
                    );
                } else if let Some((dependent_k, _)) = k_map.get(dependent_signature) {
                    // (4) set from prior strata purely for joins
                    let nested_k = Arc::new(dependent_k.enter(scope));
                    let nested_set = Arc::new(nested_k.arrange_set());
                    nest_k_map.insert(
                        Arc::clone(dependent_signature),
                        (nested_k, nested_set)
                    );
                } else {
                    // (5) rel defined from this recursive strata
                    assert!(
                        variables_map.contains_key(dependent_signature),
                        "dependent {:?} must be defined somewhere of the strata", dependent_signature
                    );
                }
            }

            // mostly identical to the non-recursive case
            for next_transformation in group_plan.strata_plan() {
                let output = next_transformation.output();
                let output_signature = output.signature();
                let (ok, ov) = output.arity();
                let target = ok + ov;

                if next_transformation.is_unary() {
                    let unary = next_transformation.unary();
                    let (ik, iv) = unary.arity();
                    let unary_signature = unary.signature();

                    // input must be in the nest_row_map or variables_map
                    let input_rel = nest_row_map
                        .get(unary_signature)
                        .map(Arc::as_ref)
                        .or_else(|| variables_map.get(unary_signature))
                        .expect(&format!("row absent for unary op: {}", unary_signature));

                    match next_transformation {
                        Transformation::Compute { .. } => {
                            assert!(ik == 0 && ok == 0);
                            let runner = Arc::clone(&runners[output_signature]);
                            let output_rel = Arc::new(codegen_row_program!());
                            nest_row_map.insert(
                                Arc::clone(output_signature),
                                output_rel,
                            );
                        },

                        Transformation::RowToRow { flow, is_no_op, .. } => { // (1) single op, tc(x, y) :- arc(y, x).
                            let output_rel =
                                if *is_no_op && nest_row_map.contains_key(unary_signature) {
                                    Arc::clone(nest_row_map.get(unary_signature).unwrap())
                                } else {
                                    Arc::new(codegen_row_row!())
                                };
                            nest_row_map.insert(Arc::clone(output_signature), output_rel);
                        },

                        Transformation::RowToK { flow, is_no_op, .. } => { // (2) leaf op for semijn or aj
                            assert!(ik == 0 && ov == 0);
                            let output_rel =
                                if *is_no_op && nest_row_map.contains_key(unary_signature) {
                                    Arc::clone(nest_row_map.get(unary_signature).unwrap())
                                } else {
                                    Arc::new(codegen_row_row!().threshold())
                                };
                            nest_k_map.insert(
                                Arc::clone(output_signature),
                                (Arc::clone(&output_rel), Arc::new(output_rel.arrange_set()))
                            );
                        },

                        Transformation::RowToKv { flow, .. } => { // (3) leaf op for jn
                            assert_eq!(ik, 0);
                            let output_kv = Arc::new(codegen_row_kv!());
                            nest_kv_map.insert(
                                Arc::clone(output_signature),
                                (Arc::clone(&output_kv), Arc::new(output_kv.arrange_dict()))
                            );
                        },

                        _ => panic!("(recursive) abnormal unary transformation"),
                    }
                } else {
                    let binary = next_transformation.binary();
                    let (ik0, mut iv0) = binary.0.arity();
                    let (ik1, mut iv1) = binary.1.arity();
                    assert_eq!(ik0, ik1);

                    let (large, small, flow) = if iv0 < iv1 {
                        std::mem::swap(&mut iv0, &mut iv1);
                        (binary.1.signature(), binary.0.signature(), &next_transformation.flow().jn_flip())
                    } else {
                        (binary.0.signature(), binary.1.signature(), next_transformation.flow())
                    };

                    let output_rel = match next_transformation {
                            Transformation::JnKvKv { .. } =>
                                kv_jn_kv(large, small, &nest_kv_map, ik0, iv0, iv1, target, flow, budget),

                            Transformation::JnKvK { .. } | Transformation::JnKKv { .. } =>
                                kv_jn_k(large, small, &nest_kv_map, &nest_k_map, ik0, iv0, iv1, target, flow, budget),

                            Transformation::JnKK { .. } =>
                                k_jn_k(large, small, &nest_k_map, ik0, iv0, iv1, target, flow, budget),

                            Transformation::Cartesian { .. } =>
                                cartesian(large, small, &nest_row_map, iv0, iv1, target, flow, budget),

                            Transformation::NjKvK { .. } =>
                                kv_aj_k(large, small, &nest_kv_map, &mut nest_k_map, ik0, iv0, iv1, target, flow, budget),

                            Transformation::NjKK { .. } =>
                                k_aj_k(large, small, &mut nest_k_map, ik0, iv0, iv1, target, flow, budget),

                            _ => panic!("(recursive) abnormal binary transformation"),
                        };

                    match (ok, ov) {
                        (0, _) => { // jn → row
                            nest_row_map.insert(Arc::clone(output_signature), Arc::clone(&output_rel));
                            // A sideways slice is not an iterative variable of this
                            // scope and the collector skips it, so a rule deriving one
                            // publishes its rows here, under the head name, where a
                            // later rule of the group reads them.
                            //
                            // This map is keyed by the roots of the group's rule plans,
                            // and a row-shaped output is not necessarily one of them: a
                            // row is also what a Cartesian product takes on both sides
                            // and what a row program reads, so an operator feeding either
                            // is an ordinary intermediate that happens to be row-shaped.
                            // A head whose rows really did go missing is still reported,
                            // by the collector that reads `last_signatures_map`.
                            if let Some(head_signatures) = group_plan
                                    .reverse_last_signatures_map()
                                    .get(output_signature)
                            {
                                for head_signature in head_signatures {
                                    if group_plan.is_sideways_head(head_signature.name()) {
                                        nest_row_map.insert(Arc::clone(head_signature), Arc::clone(&output_rel));
                                    }
                                }
                            }
                        },
                        (_, 0) => { // jn → k
                            nest_k_map.insert(
                                Arc::clone(output_signature),
                                (Arc::clone(&output_rel), Arc::new(output_rel.arrange_set()))
                            );
                        }
                        _ => { // jn → kv
                            let output_kv = Arc::new(output_rel.arrange_double(ok));
                            nest_kv_map.insert(
                                Arc::clone(output_signature),
                                (Arc::clone(&output_kv), Arc::new(output_kv.arrange_dict()))
                            );
                        }
                    }
                }
            }

            /* concatenate and threshold idbs of the recursive strata into the variables_next_map */
            recursive_collector(
                group_plan,
                &nest_row_map,
                &mut variables_next_map,
                idb_map
            );

            /* inspect idbs of the recursive strata (optional) */
            if tracing::level_enabled!(tracing::Level:: DEBUG) {
                inspector(
                    &head_signatures_set,
                    &mut variables_next_map,
                    true
                );
            }

            /* set variables and leave scope */
            let mut variables_leave_map = HashMap::with_capacity(head_signatures_set.len());
            for head_signature in head_signatures_set.iter().sorted_by_key(|sig| sig.name()) {
                let variable_next = variables_next_map
                    .remove(&Arc::clone(head_signature))
                    .expect(&format!("head missing when leave: {}", head_signature.name()));

                if let Some(variable) = variables_map.remove(&Arc::clone(head_signature)) {
                    variable.set(&variable_next); // took ownership of the variable
                } else {
                    panic!("head missing when set: {}", head_signature.name());
                }

                variables_leave_map.insert(
                    Arc::clone(head_signature),
                    variable_next.leave()
                );
            }

            /* exports */
            variables_leave_map
        });

        // final contribution of the recursive strata
        for (recursive_signature, recursive_rel) in recursive_out_map
            .into_iter()
            .sorted_by_key(|(sig, _)| sig.name().to_owned())
        {
            // if the rel is in the row_map, it will be overwritten
            row_map.insert(
                recursive_signature,
                Arc::new(recursive_rel)
            );
        }
    }
    Ok(())
}

/// A missing whole unit or individual rule contribution. Its input map is the
/// complete boundary: a contribution does not inherit its head's earlier rows.
pub struct Computation {
    pub groups: Vec<GroupStrataQueryPlan>,
    pub inputs: BTreeMap<String, Arc<RelationState>>,
    pub captures: BTreeMap<String, MaterializedUpdates>,
}

/// Everything one dataflow of an evaluation is built from.
pub struct Assembly {
    pub computations: Vec<Computation>,
    pub fat_mode: bool,
    pub idb_map: Arc<HashMap<String, AggregationHeadIDB>>,
    pub native_calls: Option<Arc<NativeCallModule>>,
    pub budget: Arc<Budget>,
    pub program_name: String,
}

impl Assembly {
    /// The union of every computation's inputs. Two computations naming one
    /// relation name one state: the stratum boundary is one boundary.
    fn shared_inputs(&self) -> BTreeMap<String, Arc<RelationState>> {
        let mut inputs = BTreeMap::new();
        for computation in &self.computations {
            for (name, state) in &computation.inputs {
                if let Some(previous) = inputs.insert(name.clone(), Arc::clone(state)) {
                    let previous: Arc<RelationState> = previous;
                    assert_eq!(
                        previous.digest, state.digest,
                        "inconsistent stratum input {name}"
                    );
                }
            }
        }
        inputs
    }

    /// Build every computation into one dataflow on `worker`, feed the
    /// worker's share of the input rows, and hand back the probe that says
    /// when the dataflow is complete. Every worker of a set calls this with
    /// the same assembly, in the same order, which is what timely requires.
    pub fn build<A: Allocate>(&self, worker: &mut Worker<A>) -> Result<ProbeHandle<Time>> {
        let peers = worker.peers();
        let index = worker.index();
        let inputs = self.shared_inputs();
        let context_native = self.native_calls.clone();

        let mut failure: Option<parsing::diagnostic::Diagnostic> = None;
        let (mut sessions, probe) = worker.dataflow::<Time, _, _>(|scope| {
            let probe = ProbeHandle::new();
            let mut sessions = Vec::new();
            let mut input_map: RowMap<_> = HashMap::new();

            for (name, state) in inputs.iter() {
                let (session, input_rel) =
                    construct_session_and_table(scope, state.arity, self.fat_mode);
                input_map.insert(
                    Arc::new(CollectionSignature::new_atom(name)),
                    Arc::new(input_rel),
                );
                sessions.push((session, Arc::clone(&state.rows)));
            }

            let context = AssemblyContext {
                fat_mode: self.fat_mode,
                native_calls: &context_native,
                idb_map: &self.idb_map,
                budget: &self.budget,
                program_name: &self.program_name,
            };

            for computation in self.computations.iter() {
                let mut row_map: RowMap<_> = computation
                    .inputs
                    .keys()
                    .map(|name| {
                        let signature = Arc::new(CollectionSignature::new_atom(name));
                        let relation = Arc::clone(&input_map[&signature]);
                        (signature, relation)
                    })
                    .collect();
                let mut kv_map: KvMap<_> = HashMap::new();
                let mut k_map: KMap<_> = HashMap::new();
                for group_plan in &computation.groups {
                    if failure.is_some() {
                        break;
                    }
                    if let Err(diagnostic) = assemble_group(
                        scope,
                        group_plan,
                        &mut row_map,
                        &mut kv_map,
                        &mut k_map,
                        &context,
                    ) {
                        failure = Some(diagnostic);
                    }
                }
                if failure.is_some() {
                    break;
                }
                for (name, updates) in &computation.captures {
                    let signature = Arc::new(CollectionSignature::new_atom(name));
                    let relation = row_map.get(&signature).unwrap_or_else(|| {
                        panic!("state boundary relation {name} is absent after its unit")
                    });
                    capture_generic(relation, Arc::clone(updates), &probe);
                }
            }

            (sessions, probe)
        });

        // Whatever happened while assembling, the sessions must be closed:
        // an open input keeps the dataflow, and with it the worker, waiting.
        for (mut session, rows) in sessions.drain(..) {
            if failure.is_none() {
                for (row_index, row) in rows.iter().enumerate() {
                    if row_index % peers == index {
                        session.update_values(row);
                    }
                }
            }
            session.close();
        }
        debug!("worker {index}/{peers}: dataflow assembled");

        match failure {
            Some(diagnostic) => Err(diagnostic),
            None => Ok(probe),
        }
    }
}

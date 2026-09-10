use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use itertools::Itertools;
use tracing::{debug, info};

extern crate timely;
extern crate differential_dataflow;

// local modules
use strata::stratification::Strata;
use planning::strata::GroupStrataQueryPlan;
use planning::transformations::Transformation;
use planning::collections::CollectionSignature;
use crate::arg::Args;
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


use macros::*;
use reading::rel::Rel::*;
use reading::rel::DoubleRel::*;
use reading::rel::{DoubleRel, Rel};
use reading::arrangements::{ArrangedDict, ArrangedSet};
use reading::reader::*; 
use reading::inspect::*;
use catalog::head::AggregationHeadIDB;

type RowMap<G> = HashMap<Arc<CollectionSignature>, Arc<Rel<G>>>;
type KvMap<G> = HashMap<Arc<CollectionSignature>, (Arc<DoubleRel<G>>, Arc<ArrangedDict<G>>)>;
type KMap<G> = HashMap<Arc<CollectionSignature>, (Arc<Rel<G>>, Arc<ArrangedSet<G>>)>;

/// Assemble one group's operators into `scope`.
///
/// Inputs are read from the maps and the group's heads are left in `row_map`
/// when it returns.  Shared by the one-shot path, which assembles every group
/// of the program into one dataflow, and by the cache path, which assembles
/// only the missed units of one stratum over injected input states.
pub(crate) fn assemble_group<G>(
    scope: &mut G,
    group_plan: &GroupStrataQueryPlan,
    row_map: &mut RowMap<G>,
    kv_map: &mut KvMap<G>,
    k_map: &mut KMap<G>,
    fat_mode: bool,
    native_calls: &Option<Arc<NativeCallModule>>,
    idb_map: &HashMap<String, AggregationHeadIDB>,
) where
    G: Scope<Timestamp = Time>,
{
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
                    Transformation::CallRowToRow { projection, .. } => {
                        assert!(ik == 0 && ok == 0);
                        let native_calls = native_calls.as_ref().expect(
                            "call projection planned without an embedded Rust module",
                        );
                        let output_rel = Arc::new(codegen_call_row!());
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
                            kv_jn_kv(large, small, kv_map, ik0, iv0, iv1, target, flow),

                        Transformation::JnKvK { .. } | Transformation::JnKKv { .. } => 
                            kv_jn_k(large, small, kv_map, k_map, ik0, iv0, iv1, target, flow),

                        Transformation::JnKK { .. } => 
                            k_jn_k(large, small, k_map, ik0, iv0, iv1, target, flow),

                        Transformation::Cartesian { .. } =>
                            cartesian(large, small, row_map, iv0, iv1, target, flow),

                        Transformation::NjKvK { .. } => 
                            kv_aj_k(large, small, kv_map, k_map, ik0, iv0, iv1, target, flow),

                        Transformation::NjKK { .. } => 
                            k_aj_k(large, small, k_map, ik0, iv0, iv1, target, flow),

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
                // (sideways) jump over sip rules
                // We do not collect sip rules in the collector, we store them in the next row map
                // TODO: temporarily way to avoid sip rule, need carefully refactor
                // to avoid this in the future
                if head_name.contains("_sip") {
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
                // (sideways) jump over sip rules
                // We do not collect sip rules in the collector, we store them in the next row map
                // TODO: temporarily way to avoid sip rule, need carefully refactor
                // to avoid this in the future
                if dependent_signature.name().contains("_sip") {
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
                        Transformation::CallRowToRow { projection, .. } => {
                            assert!(ik == 0 && ok == 0);
                            let native_calls = native_calls.as_ref().expect(
                                "call projection planned without an embedded Rust module",
                            );
                            let output_rel = Arc::new(codegen_call_row!());
                            nest_row_map.insert(
                                Arc::clone(output_signature),
                                output_rel,
                            );
                        },

                        Transformation::RowToRow { flow, is_no_op, .. } => { // (1) single op, tc(x, y) :- arc(y, x).                  
                            assert!(ik == 0 && ok == 0);
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
                                kv_jn_kv(large, small, &nest_kv_map, ik0, iv0, iv1, target, flow),

                            Transformation::JnKvK { .. } | Transformation::JnKKv { .. } => 
                                kv_jn_k(large, small, &nest_kv_map, &nest_k_map, ik0, iv0, iv1, target, flow),

                            Transformation::JnKK { .. } => 
                                k_jn_k(large, small, &nest_k_map, ik0, iv0, iv1, target, flow),

                            Transformation::Cartesian { .. } =>
                                cartesian(large, small, &nest_row_map, iv0, iv1, target, flow),

                            Transformation::NjKvK { .. } => 
                                kv_aj_k(large, small, &nest_kv_map, &mut nest_k_map, ik0, iv0, iv1, target, flow),

                            Transformation::NjKK { .. } => 
                                k_aj_k(large, small, &mut nest_k_map, ik0, iv0, iv1, target, flow),

                            _ => panic!("(recursive) abnormal binary transformation"),
                        };

                    match (ok, ov) {
                        (0, _) => { // jn → row
                            nest_row_map.insert(Arc::clone(output_signature), Arc::clone(&output_rel));
                            // (sideways) compensate sip rules
                            //
                            // A sip head is not an iterative variable of this scope and the
                            // collector skips it, so a rule deriving one publishes its rows here,
                            // under the head name, where a later rule of the stratum reads them.
                            // TODO: temporarily way to avoid sip rule, need carefully refactor
                            // to avoid this in the future
                            //
                            // This map is keyed by the *roots* of the stratum's rule plans, and a
                            // row-shaped output is not necessarily one of them. A row is also what
                            // a Cartesian product takes on both sides and what a row-local call
                            // projection takes as input, so an operator feeding either is an
                            // ordinary intermediate that happens to be row-shaped. It is the root
                            // of no rule, it names no head, and there is nothing to compensate:
                            //
                            //     H(x, y, z) :- H(x, w, q), A(w, y), C(z).
                            //
                            // plans the join of H and A as a row for the product with C, and
                            // demanding a head for it aborted the whole program.
                            //
                            // Proceeding here cannot hide a head. A head whose rows really did go
                            // missing is still reported, by the collector that reads
                            // `last_signatures_map` for that head, and by the operator that reads
                            // a sip name it was promised.
                            if let Some(head_signatures) = group_plan
                                    .reverse_last_signatures_map()
                                    .get(output_signature)
                            {
                                for head_signature in head_signatures {
                                    if head_signature.name().contains("_sip") {
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
            // debug!("last_signatures_map: {:?}", group_plan.last_signatures_map());
            recursive_collector(
                group_plan.last_signatures_map(), 
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
}

/// The one-shot evaluation: every group of the program in one dataflow, inputs
/// read from the fact files, outputs written by the dataflow's own sinks.
pub fn program_execution(
    args: Args,
    strata: Strata,
    group_plans: Vec<GroupStrataQueryPlan>,
    fat_mode: bool,
    idb_map: HashMap<String, AggregationHeadIDB>,
) {
    let timely_args = args.timely_args();
    let native_calls = strata
        .program()
        .embedded_rust()
        .map(|embedded| NativeCallModule::compile_and_load(embedded, args.call_cache()))
        .transpose()
        .unwrap_or_else(|error| panic!("failed to prepare .code rust module: {error}"));
    let output_paths = args.csvs().map(|csv_path| {
        strata
            .program()
            .idbs()
            .iter()
            .map(|relation| format!("{}/csvs/{}.csv", csv_path, relation.name()))
            .collect::<Vec<_>>()
    });
    let size_output_path = args
        .csvs()
        .map(|csv_path| format!("{}/csvs/size.txt", csv_path));
    let recursive_output_names = group_plans
        .iter()
        .filter(|group_plan| group_plan.is_recursive())
        .flat_map(|group_plan| group_plan.heads().into_keys())
        .collect::<HashSet<_>>();
    let timer = ::std::time::Instant::now();
    let worker_timer = timer;

    let guards = timely::execute_from_args(timely_args.into_iter(), move |worker| {
        let timer = worker_timer;
        let peers = worker.peers();
        let id = worker.index(); 

        /* assemble dataflow */
        let mut session_map = worker.dataflow::<Time, _, _>(|scope| {
            let mut session_map = HashMap::new();          // map from each edb name to input session (for data loading)
            let mut row_map: RowMap<_> = HashMap::new();   // map from row signature (edbs and idbs) to the physical dataflow data
            let mut kv_map: KvMap<_> = HashMap::new();     // map from (k, v) signature to the physical dataflow data   
            let mut k_map: KMap<_> = HashMap::new();       // map from (k, ) signature to the physical dataflow data

            /* construct dataflow rels & input session (i.e., file handles) to load the input */
            for edb in strata.program().edbs() {
                let edb_name = edb.name();
                let (session_generic, input_rel) = construct_session_and_table(scope, edb.arity(), fat_mode);
                    
                session_map.insert(
                    edb_name.to_string(), session_generic
                );

                row_map.insert(
                    Arc::new(CollectionSignature::new_atom(edb_name)), Arc::new(input_rel)
                );
            }

            /* inspect edbs (optional) */
            if tracing::level_enabled!(tracing::Level::DEBUG) {
                for (signature, rel) in row_map
                    .iter()
                    .sorted_by_key(|(signature, _)| signature.name()) {
                    printsize_generic(rel, &format!("[{}]", signature.name()), false);
                }
            }

            for group_plan in group_plans.iter() {
                assemble_group(
                    scope,
                    group_plan,
                    &mut row_map,
                    &mut kv_map,
                    &mut k_map,
                    fat_mode,
                    &native_calls,
                    &idb_map,
                );
            } // end of a strata (group plan)

            for idb in strata.program().idbs() {
                let rel_name = idb.name();
                let signature = Arc::new(CollectionSignature::new_atom(rel_name));
                let Some(rel) = row_map.get(&signature) else {
                    continue;
                };

                printsize_generic(
                    rel,
                    &format!("[{}]", rel_name),
                    recursive_output_names.contains(rel_name),
                );
                if let Some(csv_path) = args.csvs() {
                    writesize_generic(
                        rel,
                        rel_name,
                        &format!("{}/csvs/size.txt", csv_path),
                        id,
                    );
                    write_generic(
                        rel,
                        &format!("{}/csvs/{}.csv", csv_path, rel_name),
                        id,
                        args.delimiter().as_bytes()[0],
                    );
                }
            }

            /* exports */
            session_map
        }); 

        if id == 0 {
            info!("{:?}:\tDataflow assembled", timer.elapsed());
        }

        /* feeding edb data */ 
        for rel_decl in strata.program().edbs() {
            let rel_name = rel_decl.name();
            let rel_path =     
                if let Some(path) = rel_decl.path() {
                    format!("{}/{}", args.facts(), path)
                } else {
                    format!("{}/{}.facts", args.facts(), rel_name)
                };
                
            let session_generic = session_map
                .get_mut(rel_name)
                .expect(&format!("entry from session_map: {}", rel_name));
            
            read_row_generic(
                rel_decl, 
                &rel_path, 
                &args.delimiter().as_bytes()[0], 
                session_generic, 
                id, 
                peers,
                fat_mode
            );
        }

        for rel_decl in strata.program().edbs() {
            let rel_name = rel_decl.name();
            session_map
                .remove(rel_name)
                .expect(&format!("entry from session_map: {}", rel_name))
                .close();

            if id == 0 {
                info!("{:?}:\tData loaded for {}", timer.elapsed(), rel_name);
            }
        }

        /* executing the dataflow */
        while worker.step() {
            // spinning
        }
    }).expect("execute_from_args dies");

    let peers = guards.guards().len();
    for result in guards.join() {
        result.expect("timely worker panicked");
    }

    info!("{:?}:\tDataflow executed", timer.elapsed());
    if let Some(output_paths) = output_paths {
        for output_path in output_paths {
            debug!("merging worker partitions into {}", output_path);
            merge_relation_partitions(&output_path, peers);
        }
    }
    if let Some(size_output_path) = size_output_path {
        merge_relation_partitions(&size_output_path, peers);
    }
}

/// A missing whole unit or individual rule contribution. Its input map is the
/// complete boundary: a contribution does not inherit its head's earlier rows.
pub(crate) struct StratumComputation {
    pub groups: Vec<GroupStrataQueryPlan>,
    pub inputs: BTreeMap<String, Arc<RelationState>>,
    pub captures: BTreeMap<String, MaterializedUpdates>,
}

/// The missing computations of one stratum, in one dataflow. Input collections
/// are shared, but each computation has its own relation and arrangement maps:
/// two rules contributing to the same head must not capture each other's rows.
pub(crate) fn stratum_execution(
    args: &Args,
    computations: Vec<StratumComputation>,
    fat_mode: bool,
    idb_map: Arc<HashMap<String, AggregationHeadIDB>>,
    native_calls: Option<Arc<NativeCallModule>>,
) {
    let timely_args = args.timely_args();
    let mut inputs = BTreeMap::new();
    for computation in &computations {
        for (name, state) in &computation.inputs {
            if let Some(previous) = inputs.insert(name.clone(), Arc::clone(state)) {
                assert_eq!(
                    previous.digest, state.digest,
                    "inconsistent stratum input {name}"
                );
            }
        }
    }
    let computations = Arc::new(computations);
    let inputs = Arc::new(inputs);

    let guards = timely::execute_from_args(timely_args.into_iter(), move |worker| {
        let peers = worker.peers();
        let id = worker.index();

        let mut sessions = worker.dataflow::<Time, _, _>(|scope| {
            let mut sessions = Vec::new();
            let mut input_map: RowMap<_> = HashMap::new();

            for (name, state) in inputs.iter() {
                let (session, input_rel) =
                    construct_session_and_table(scope, state.arity, fat_mode);
                input_map.insert(
                    Arc::new(CollectionSignature::new_atom(name)),
                    Arc::new(input_rel),
                );
                sessions.push((session, Arc::clone(&state.rows)));
            }

            for computation in computations.iter() {
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
                    assemble_group(
                        scope,
                        group_plan,
                        &mut row_map,
                        &mut kv_map,
                        &mut k_map,
                        fat_mode,
                        &native_calls,
                        &idb_map,
                    );
                }
                for (name, updates) in &computation.captures {
                    let signature = Arc::new(CollectionSignature::new_atom(name));
                    let relation = row_map.get(&signature).unwrap_or_else(|| {
                        panic!("state boundary relation {name} is absent after its unit")
                    });
                    capture_generic(relation, Arc::clone(updates));
                }
            }

            sessions
        });

        for (mut session, rows) in sessions.drain(..) {
            for (row_index, row) in rows.iter().enumerate() {
                if row_index % peers == id {
                    session.update_values(row);
                }
            }
            session.close();
        }

        while worker.step() {
            // spinning
        }
    })
    .expect("execute_from_args dies");

    for result in guards.join() {
        result.expect("timely worker panicked");
    }
}

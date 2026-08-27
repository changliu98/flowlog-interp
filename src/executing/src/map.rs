use planning::constraints::BaseConstraints;
use std::sync::Arc;
use arrayvec::ArrayVec;
use planning::arguments::TransformationArgument;
use planning::flow::TransformationFlow;
use reading::row::Row;
use reading::row::FatRow;
use reading::row::Array;
use reading::Val;
use crate::compare::*;
use crate::native_calls::NativeCallModule;
use planning::compare::ComparisonExprArgument;
use planning::calls::CallProjection;


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
fn is_filtered<const M: usize>(v: &Row<M>, const_eqs: &[(usize, Val)], var_eqs: &[(usize, usize)], compares: &Vec<ComparisonExprArgument>) -> bool {
    const_eqs.iter().all(|(i, constant)| v.column(*i) == *constant) && 
    var_eqs.iter().all(|(i, j)| v.column(*i) == v.column(*j)) &&
    compares.iter().all(|compare| compare_row(v, compare))
}

pub fn row_row<const M: usize, const N: usize>(flow: &TransformationFlow) -> impl FnMut(Row<M>) -> Option<Row<N>> {
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

    #[inline(always)]
    move |v| 
    if is_filtered(&v, &const_eqs, &var_eqs, &compares) {
        let mut row = Row::<N>::new();
        for id in &k_or_v_ids { row.push(v.column(*id)); }
        Some(row)
    } else {
        None
    }
}

/// Render the typed call program once, then execute it as one row-local
/// map/filter. Native symbols are resolved here, not for each tuple.
pub fn call_row<const M: usize, const N: usize>(
    projection: &CallProjection,
    native_calls: &Arc<NativeCallModule>,
) -> impl FnMut(Row<M>) -> Option<Row<N>> {
    assert_eq!(projection.input_variables().len(), M);
    assert_eq!(projection.head().len(), N);
    let resolved = native_calls.resolve(projection);
    move |input| {
        resolved.evaluate(&input).map(|values| {
            let mut output = Row::<N>::new();
            for value in values {
                output.push(value);
            }
            output
        })
    }
}


/* ------------------------------------------------------------------------ */
/* renders for map from row to kv */
/* ------------------------------------------------------------------------ */
pub fn row_kv<const M: usize, const K: usize, const V: usize>(flow: &TransformationFlow) -> impl FnMut(Row<M>) -> Option<(Row<K>, Row<V>)> {
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

    #[inline(always)]
    move |v| 
    if is_filtered(&v, &const_eqs, &var_eqs, &compares) {
        let mut key = Row::<K>::new();
        let mut value = Row::<V>::new();
        for id in &kids { key.push(v.column(*id)); }
        for id in &vids { value.push(v.column(*id)); }

        Some((key, value))
    } else {
        None
    }
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
fn is_filtered_fat(v: &FatRow, const_eqs: &[(usize, Val)], var_eqs: &[(usize, usize)], compares: &Vec<ComparisonExprArgument>) -> bool {
    const_eqs.iter().all(|(i, constant)| v.column(*i) == *constant) && 
    var_eqs.iter().all(|(i, j)| v.column(*i) == v.column(*j)) &&
    compares.iter().all(|compare| compare_row(v, compare))
}

pub fn row_row_fat(flow: &TransformationFlow) -> impl FnMut(FatRow) -> Option<FatRow> {
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

    #[inline(always)]
    move |v| 
    if is_filtered_fat(&v, &const_eqs, &var_eqs, &compares) {
        let mut row = FatRow::new();
        for id in &k_or_v_ids { row.push(v.column(*id)); }
        Some(row)
    } else {
        None
    }
}

pub fn call_row_fat(
    projection: &CallProjection,
    native_calls: &Arc<NativeCallModule>,
) -> impl FnMut(FatRow) -> Option<FatRow> {
    let resolved = native_calls.resolve(projection);
    move |input| {
        resolved.evaluate(&input).map(|values| {
            let mut output = FatRow::new();
            for value in values {
                output.push(value);
            }
            output
        })
    }
}

/* ------------------------------------------------------------------------ */
/* renders for map from fat row to fat kv */
/* ------------------------------------------------------------------------ */
pub fn row_kv_fat(flow: &TransformationFlow) -> impl FnMut(FatRow) -> Option<(FatRow, FatRow)> {
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

    #[inline(always)]
    move |v| 
    if is_filtered_fat(&v, &const_eqs, &var_eqs, &compares) {
        let mut key = FatRow::new();
        let mut value = FatRow::new();
        for id in &kids { key.push(v.column(*id)); }
        for id in &vids { value.push(v.column(*id)); }

        Some((key, value))
    } else {
        None
    }
}


use timely::progress::Timestamp;
use std::sync::Arc;
use std::collections::HashMap;
use planning::flow::TransformationFlow;
use planning::collections::CollectionSignature;
use reading::rel::DoubleRel;
use reading::arrangements::ArrangedDict;
use reading::arrangements::ArrangedSet;
use reading::rel::Rel::*;
use reading::rel::Rel;
use reading::row::FatRow;
use reading::Semiring;

use differential_dataflow::collection::VecCollection;
use differential_dataflow::lattice::Lattice;
use timely::order::TotalOrder;
use differential_dataflow::Data;
use macros::*;
use crate::accounting::Budget;
use crate::jn::*;

/// The Cartesian product of two relations, over fat rows.
///
/// This is the general implementation: fat rows carry any arity, so it is
/// written once and serves both the globally fat mode and the fallback taken by
/// `codegen_cartesian` for the shapes its fixed-size table does not generate.
fn cartesian_fat_rows<'scope, T: Timestamp>(
    rel_0: VecCollection<'scope, T, FatRow, Semiring>,
    rel_1: VecCollection<'scope, T, FatRow, Semiring>,
    flow: &TransformationFlow,
    budget: &Arc<Budget>,
) -> VecCollection<'scope, T, FatRow, Semiring>
where
    T: Data + Lattice + TotalOrder,
{
    rel_0.map(|row| ((), row)).arrange_by_key().join_core(
        rel_1.map(|row| ((), row)).arrange_by_key(),
        cartesian_logic_fat(flow, budget),
    )
}

pub fn cartesian<'scope, T: Timestamp>(
    large: &Arc<CollectionSignature>,
    small: &Arc<CollectionSignature>,
    row_map: &HashMap<Arc<CollectionSignature>, Arc<Rel<'scope, T>>>,
    iv0: usize,
    iv1: usize,
    target: usize,
    flow: &TransformationFlow,
    budget: &Arc<Budget>,
) -> Arc<Rel<'scope, T>>
where
    T: Data+Lattice+TotalOrder,
{
    let rel_0 = row_map.get(large).expect("0 for cartesian");
    let rel_1 = row_map.get(small).expect("1 for cartesian");
    Arc::new(codegen_cartesian!())
}


pub fn kv_jn_kv<'scope, T: Timestamp>(
    large: &Arc<CollectionSignature>, 
    small: &Arc<CollectionSignature>, 
    kv_map: &HashMap<Arc<CollectionSignature>, (Arc<DoubleRel<'scope, T>>, Arc<ArrangedDict<'scope, T>>)>,
    ik0: usize,
    iv0: usize,
    iv1: usize,
    target: usize,
    flow: &TransformationFlow,
    budget: &Arc<Budget>,
) -> Arc<Rel<'scope, T>>
where
    T: Data+Lattice+TotalOrder,
{
    let (_, dict_0) = kv_map.get(large).expect("0 for kv jn kv");
    let (_, dict_1) = kv_map.get(small).expect("1 for kv jn kv");
    Arc::new(codegen_jn!())
}    


pub fn kv_jn_k<'scope, T: Timestamp>(
    large: &Arc<CollectionSignature>, 
    small: &Arc<CollectionSignature>, 
    kv_map: &HashMap<Arc<CollectionSignature>, (Arc<DoubleRel<'scope, T>>, Arc<ArrangedDict<'scope, T>>)>,
    k_map: &HashMap<Arc<CollectionSignature>, (Arc<Rel<'scope, T>>, Arc<ArrangedSet<'scope, T>>)>,
    ik0: usize,
    iv0: usize,
    iv1: usize,
    target: usize,
    flow: &TransformationFlow,
    budget: &Arc<Budget>,
) -> Arc<Rel<'scope, T>>
where
    T: Data+Lattice+TotalOrder,
{
    assert!(iv1 == 0);
    let (_, dict_0) = kv_map.get(large).expect("dict for kv jn k");
    let (_, set_1) = k_map.get(small).expect("set for kv jn k");
    Arc::new(codegen_kv_k_jn!())
}


pub fn k_jn_k<'scope, T: Timestamp>(
    large: &Arc<CollectionSignature>, 
    small: &Arc<CollectionSignature>, 
    k_map: &HashMap<Arc<CollectionSignature>, (Arc<Rel<'scope, T>>, Arc<ArrangedSet<'scope, T>>)>,
    ik0: usize,
    iv0: usize,
    iv1: usize,
    target: usize,
    flow: &TransformationFlow,
    budget: &Arc<Budget>,
) -> Arc<Rel<'scope, T>>
where
    T: Data+Lattice+TotalOrder,
{
    assert!(iv0 == 0 && iv1 == 0);
    let (_, set_0) = k_map.get(large).expect("0 for k jn k");
    let (_, set_1) = k_map.get(small).expect("1 for k jn k");
    Arc::new(codegen_k_k_jn!())
}


pub fn kv_aj_k<'scope, T: Timestamp>(
    large: &Arc<CollectionSignature>, 
    small: &Arc<CollectionSignature>, 
    kv_map: &HashMap<Arc<CollectionSignature>, (Arc<DoubleRel<'scope, T>>, Arc<ArrangedDict<'scope, T>>)>,
    k_map: &mut HashMap<Arc<CollectionSignature>, (Arc<Rel<'scope, T>>, Arc<ArrangedSet<'scope, T>>)>,
    ik0: usize,
    iv0: usize,
    iv1: usize,
    target: usize,
    flow: &TransformationFlow,
    budget: &Arc<Budget>,
) -> Arc<Rel<'scope, T>>
where
    T: Data+Lattice+TotalOrder,
{
    assert!(iv1 == 0);
    if let Some((rel_1, set_1)) = k_map.get_mut(small) {
        *rel_1 = Arc::new(set_1.threshold()); 
        *set_1 = Arc::new(rel_1.arrange_set());
    } else { 
        panic!("threshold for kv aj k"); 
    }
    let (_, dict_0) = kv_map.get(large).expect("dict for kv aj k");
    let (_, set_1) = k_map.get(small).expect("set for kv aj k");
    let _ = budget;

    Arc::new(codegen_kv_antijoin!())
}


pub fn k_aj_k<'scope, T: Timestamp>(
    large: &Arc<CollectionSignature>, 
    small: &Arc<CollectionSignature>, 
    k_map: &mut HashMap<Arc<CollectionSignature>, (Arc<Rel<'scope, T>>, Arc<ArrangedSet<'scope, T>>)>,
    ik0: usize,
    iv0: usize,
    iv1: usize,
    target: usize,
    flow: &TransformationFlow,
    budget: &Arc<Budget>,
) -> Arc<Rel<'scope, T>>
where
    T: Data+Lattice+TotalOrder,
{
    assert!(iv0 == 0 && iv1 == 0);
    if let Some((rel_1, set_1)) = k_map.get_mut(small) {
        *rel_1 = Arc::new(set_1.threshold()); 
        *set_1 = Arc::new(rel_1.arrange_set());
    } else { 
        panic!("threshold for k aj k"); 
    }
    let (_, set_0) = k_map.get(large).expect("0 for k aj k");
    let (_, set_1) = k_map.get(small).expect("1 for k aj k");
    let _ = budget;

    Arc::new(codegen_k_antijoin!())
}


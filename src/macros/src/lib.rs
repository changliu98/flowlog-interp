use itertools::iproduct;
use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::Ident;

// Import centralized configuration constants
use reading::config::{KV_MAX, PROD_MAX, ROW_MAX};

/* ------------------------------------------------------------------------ */
/* the shapes each operator generates a fixed-size arm for */
/* ------------------------------------------------------------------------ */
//
// Every dispatch below is a `match` over runtime arities with a `_` arm that
// panics, so a shape absent from one of these spaces is a legal program refused
// by arity. Which shapes must be present is not a free choice: `should_use_fat_
// mode` plans a collection onto fat rows only when the fixed-size rows cannot
// hold it, and everything it leaves behind arrives here. The two limits differ
// and are not interchangeable - a row reaches `ROW_MAX`, while each half of a
// (key, value) pair is bounded by the generated key/value tables at `KV_MAX` -
// and reading one for the other is what produced
// `codegen_aggregation unimplemented for arity 6`. The spaces are named so that
// `every_dispatch_covers_the_shapes_planned_onto_fixed_size_rows` can check
// them against that invariant rather than against a second copy of themselves.

/// row -> row: any row in, any row out.
fn row_row_space() -> Vec<(usize, usize)> {
    iproduct!(1..=ROW_MAX, 1..=ROW_MAX).collect()
}

/// row -> (key, value): a row split into two halves of the key/value tables.
/// A split cannot produce more columns than the row it reads.
fn row_kv_space() -> Vec<(usize, usize, usize)> {
    iproduct!(1..=ROW_MAX, 1..=KV_MAX, 1..=KV_MAX)
        .filter(|&(iv, ok, ov)| iv >= ok + ov)
        .collect()
}

/// (key, value) join (key, value) -> row. The dispatch site orders the operands
/// by value arity, so only `iv0 >= iv1` can arrive.
fn jn_space() -> Vec<(usize, usize, usize, usize)> {
    iproduct!(1..=KV_MAX, 1..=KV_MAX, 1..=KV_MAX, 1..=ROW_MAX)
        .filter(|&(_, iv0, iv1, _)| iv0 >= iv1)
        .collect()
}

/// (key, value) join (key,) -> row.
fn kv_k_jn_space() -> Vec<(usize, usize, usize)> {
    iproduct!(1..=KV_MAX, 1..=KV_MAX, 1..=ROW_MAX).collect()
}

/// (key,) join (key,) -> row.
fn k_k_jn_space() -> Vec<(usize, usize)> {
    iproduct!(1..=KV_MAX, 1..=ROW_MAX).collect()
}

/// (∅, value) product (∅, value) -> row, bounded by `PROD_MAX` rather than by
/// the rows. This one table is deliberately partial: the dispatch falls back to
/// the fat-row product for every shape outside it, so the arms here are a fast
/// path and not the operator's domain.
fn cartesian_space() -> Vec<(usize, usize, usize)> {
    iproduct!(1..=PROD_MAX, 1..=PROD_MAX, 1..=PROD_MAX)
        .filter(|&(iv0, iv1, target)| iv0 + iv1 >= target)
        .collect()
}

/// (key, value) -> row. The result is a row, so it is bounded by `ROW_MAX` and
/// not by the key/value table it is flattened from; it is also a projection of
/// the key and value, so it cannot be wider than both together.
fn kv_flatten_space() -> Vec<(usize, usize, usize)> {
    iproduct!(1..=KV_MAX, 1..=KV_MAX, 1..=ROW_MAX)
        .filter(|&(ik, iv, target)| ik + iv >= target)
        .collect()
}

/// (key,) -> row, a projection of the key.
fn k_flatten_space() -> Vec<(usize, usize)> {
    iproduct!(1..=KV_MAX, 1..=KV_MAX)
        .filter(|&(ik, target)| ik >= target)
        .collect()
}

/// Group-by columns of an aggregate; the relation's arity is one more. The
/// group-by key is an ordinary fixed-size row that `reduce_core` arranges by
/// itself rather than half of a generated join table, so the bound is the row's.
fn aggregation_space() -> Vec<usize> {
    (0..ROW_MAX).collect()
}

/// Relation arity of a min aggregate on the specialised semiring path.
fn min_optimize_space() -> Vec<usize> {
    (1..=ROW_MAX).collect()
}

/* ------------------------------------------------------------------------ */
/* codegen for maps */
/* ------------------------------------------------------------------------ */

/* row → row */
#[proc_macro]
pub fn codegen_row_row(_: TokenStream) -> TokenStream {
    let space = row_row_space();
    let mut arms = vec![];
    for (iv_, target_) in space {
        let base_type = Ident::new(&format!("rel_{}", iv_), Span::call_site());
        let final_rel = Ident::new(&format!("Collection{}", target_), Span::call_site());
        arms.push(quote! {
            (#iv_, #target_) => #final_rel(
                input_rel.#base_type().flat_map(row_row::<#iv_, #target_>(flow)))
        });
    }

    let expanded = quote! {
        if input_rel.is_fat() {
            CollectionFat(
                input_rel.rel_fat().flat_map(row_row_fat(flow)),
                target
            )
        } else {
            match (iv, target) {
                #(#arms),*,
                _ => panic!("codegen_row_row unimplemented for {}, {}", iv, target),
            }
        }
    };

    TokenStream::from(expanded)
}

/* embedded Rust call projection: row → row */
#[proc_macro]
pub fn codegen_call_row(_: TokenStream) -> TokenStream {
    let space = row_row_space();
    let mut arms = vec![];
    for (iv_, target_) in space {
        let base_type = Ident::new(&format!("rel_{}", iv_), Span::call_site());
        let final_rel = Ident::new(&format!("Collection{}", target_), Span::call_site());
        arms.push(quote! {
            (#iv_, #target_) => #final_rel(
                input_rel.#base_type().flat_map(
                    call_row::<#iv_, #target_>(projection, native_calls)
                )
            )
        });
    }

    let expanded = quote! {
        if input_rel.is_fat() {
            CollectionFat(
                input_rel.rel_fat().flat_map(call_row_fat(projection, native_calls)),
                target
            )
        } else {
            match (iv, target) {
                #(#arms),*,
                _ => panic!("codegen_call_row unimplemented for {}, {}", iv, target),
            }
        }
    };

    TokenStream::from(expanded)
}

/* row → kv */
#[proc_macro]
pub fn codegen_row_kv(_: TokenStream) -> TokenStream {
    let space = row_kv_space();
    let mut arms = vec![];

    for (iv_, ok_, ov_) in space {
        let base_type = Ident::new(&format!("rel_{}", iv_), Span::call_site());
        let final_double_rel = Ident::new(&format!("DoubleRel{}_{}", ok_, ov_), Span::call_site());
        arms.push(quote! {
            (#iv_, #ok_, #ov_) => #final_double_rel(
                input_rel.#base_type()
                         .flat_map(row_kv::<#iv_, #ok_, #ov_>(flow))
                        )
        });
    }

    let expanded = quote! {
        if input_rel.is_fat() {
            DoubleRelFat(
                input_rel.rel_fat().flat_map(row_kv_fat(flow)),
                ok, // key arity
                ov  // value arity
            )
        } else {
            match (iv, ok, ov) {
                #(#arms),*,
                _ => panic!("codegen_row_kv unimplemented for {}, {}, {}", iv, ok, ov),
            }
        }
    };

    TokenStream::from(expanded)
}

/* ------------------------------------------------------------------------ */
/* codegen for kv ⋈ kv */
/* ------------------------------------------------------------------------ */
#[proc_macro]
pub fn codegen_jn(_: TokenStream) -> TokenStream {
    let space = jn_space();
    let mut arms = vec![];

    for (ik0_, iv0_, iv1_, target_) in space {
        let type_0 = Ident::new(&format!("dict_{}_{}", ik0_, iv0_), Span::call_site());
        let type_1 = Ident::new(&format!("dict_{}_{}", ik0_, iv1_), Span::call_site());
        let final_rel = Ident::new(&format!("Collection{}", target_), Span::call_site());
        arms.push(quote! {
            (#ik0_, #iv0_, #iv1_, #target_) => {
                #final_rel(
                    dict_0.#type_0()
                    .join_core(
                        dict_1.#type_1(),
                        jn_logic::<#ik0_, #iv0_, #iv1_, #target_>(flow)
                    )
                )
            }
        });
    }

    let expanded = quote! {
        if dict_0.is_fat() && dict_1.is_fat() {
            CollectionFat(
                dict_0.dict_fat()
                    .join_core(
                        dict_1.dict_fat(),
                        jn_logic_fat(flow)
                    ),
                target
            )
        } else {
            match (ik0, iv0, iv1, target) {
                #(#arms),*,
                _ => panic!("codegen_jn unimplemented for {}, {}, {}, {}", ik0, iv0, iv1, target),
            }
        }
    };

    TokenStream::from(expanded)
}

#[proc_macro]
pub fn codegen_cartesian(_: TokenStream) -> TokenStream {
    let space = cartesian_space();
    let mut arms = vec![];

    for (iv0_, iv1_, target_) in space {
        let type_0 = Ident::new(&format!("rel_{}", iv0_), Span::call_site());
        let type_1 = Ident::new(&format!("rel_{}", iv1_), Span::call_site());
        let final_rel = Ident::new(&format!("Collection{}", target_), Span::call_site());
        arms.push(quote! {
            (#iv0_, #iv1_, #target_) => {
                #final_rel(
                    rel_0.#type_0()
                         .map(|x| ((), x))
                         .arrange_by_key()
                         .join_core(
                            &rel_1.#type_1()
                                  .map(|x| ((), x))
                                  .arrange_by_key(),
                                cartesian_logic::<#iv0_, #iv1_, #target_>(flow)
                         )
                )
            }
        });
    }

    let expanded = quote! {
        if rel_0.is_fat() && rel_1.is_fat() {
            CollectionFat(
                cartesian_fat_rows(rel_0.rel_fat(), rel_1.rel_fat(), flow),
                target
            )
        } else {
            match (iv0, iv1, target) {
                #(#arms),*,
                // The fixed-size arms above reach `PROD_MAX`, which is narrower
                // than the row representation itself. Every wider shape is a
                // legal cross-join, so it is computed on fat rows and narrowed
                // back, making the operator total over every arity the engine
                // supports rather than over `PROD_MAX` alone.
                _ => Rel::from_fat_rows(
                    cartesian_fat_rows(&rel_0.to_fat_rows(), &rel_1.to_fat_rows(), flow),
                    target,
                ),
            }
        }
    };

    TokenStream::from(expanded)
}

/* ------------------------------------------------------------------------ */
/* codegen for kv ⋈ k */
/* ------------------------------------------------------------------------ */
#[proc_macro]
pub fn codegen_kv_k_jn(_: TokenStream) -> TokenStream {
    let space = kv_k_jn_space();

    let mut arms = vec![];
    for (ik0_, iv0_, target_) in space {
        let type_0 = Ident::new(&format!("dict_{}_{}", ik0_, iv0_), Span::call_site());
        let type_1 = Ident::new(&format!("set_{}", ik0_), Span::call_site());
        let final_rel = Ident::new(&format!("Collection{}", target_), Span::call_site());
        arms.push(quote! {
            (#ik0_, #iv0_, #target_) => {
                #final_rel(
                    dict_0.#type_0()
                    .join_core(
                        set_1.#type_1(),
                        v1_jn_logic::<#ik0_, #iv0_, #target_>(flow)
                    )
                )
            }
        });
    }

    let expanded = quote! {
        if dict_0.is_fat() && set_1.is_fat() {
            CollectionFat(
                dict_0.dict_fat()
                    .join_core(
                        set_1.set_fat(),
                        v1_jn_logic_fat(flow)
                    ),
                target
            )
        } else {
            match (ik0, iv0, target) {
                #(#arms),*,
                _ => panic!("cpdegen_kv_k_jn unimplemented for {}, {}, {}", ik0, iv0, target),
            }
        }
    };

    TokenStream::from(expanded)
}

/* ------------------------------------------------------------------------ */
/* codegen for k ⋈ k */
/* ------------------------------------------------------------------------ */
#[proc_macro]
pub fn codegen_k_k_jn(_: TokenStream) -> TokenStream {
    let space = k_k_jn_space();

    let mut arms = vec![];
    for (ik0_, target_) in space {
        let type_0 = Ident::new(&format!("set_{}", ik0_), Span::call_site());
        let type_1 = Ident::new(&format!("set_{}", ik0_), Span::call_site());
        let final_rel = Ident::new(&format!("Collection{}", target_), Span::call_site());
        arms.push(quote! {
            (#ik0_, #target_) => {
                #final_rel(
                    set_0.#type_0()
                    .join_core(
                        set_1.#type_1(),
                        v2_jn_logic::<#ik0_, #target_>(flow)
                    )
                )
            }
        });
    }

    let expanded = quote! {
        if set_0.is_fat() && set_1.is_fat() {
            CollectionFat(
                set_0.set_fat()
                    .join_core(
                        set_1.set_fat(),
                        v2_jn_logic_fat(flow)
                    ),
                target
            )
        } else {
            match (ik0, target) {
                #(#arms),*,
                _ => panic!("codegen_k_k_jn unimplemented for {}, {}", ik0, target),
            }
        }
    };

    TokenStream::from(expanded)
}

/* ------------------------------------------------------------------------ */
/* codegen for aj flatten */
/* ------------------------------------------------------------------------ */

#[proc_macro]
pub fn codegen_kv_flatten(_: TokenStream) -> TokenStream {
    let space = kv_flatten_space();

    let mut arms = vec![];
    for (ik0_, iv0_, target_) in space {
        let type_0 = Ident::new(&format!("dict_{}_{}", ik0_, iv0_), Span::call_site());
        let final_rel = Ident::new(&format!("Collection{}", target_), Span::call_site());
        arms.push(quote! {
            (#ik0_, #iv0_, #target_) => {
                #final_rel(
                    dict_0.#type_0().as_collection(aj_flatten::<#ik0_, #iv0_, #target_>(flow))
                )
            }
        });
    }

    let expanded = quote! {
        if dict_0.is_fat() {
            CollectionFat(
                dict_0.dict_fat().as_collection(aj_flatten_fat(flow)),
                target
            )
        } else {
            match (ik0, iv0, target) {
                #(#arms),*,
                _ => panic!("codegen_kv_flatten unimplemented for {}, {}, {}", ik0, iv0, target),
            }
        }
    };

    TokenStream::from(expanded)
}

#[proc_macro]
pub fn codegen_k_flatten(_: TokenStream) -> TokenStream {
    let space = k_flatten_space();

    let mut arms = vec![];
    for (ik0_, target_) in space {
        let type_0 = Ident::new(&format!("set_{}", ik0_), Span::call_site());
        let final_rel = Ident::new(&format!("Collection{}", target_), Span::call_site());
        arms.push(quote! {
            (#ik0_, #target_) => {
                #final_rel(
                    set_0.#type_0().as_collection(v1_aj_flatten::<#ik0_, #target_>(flow))
                )
            }
        });
    }

    let expanded = quote! {
        if set_0.is_fat() {
            CollectionFat(
                set_0.set_fat().as_collection(v1_aj_flatten_fat(flow)),
                target
            )
        } else {
            match (ik0, target) {
                #(#arms),*,
                _ => panic!("codegen_k_flatten unimplemented for {}, {}", ik0, target),
            }
        }
    };

    TokenStream::from(expanded)
}

/* ------------------------------------------------------------------------ */
/* codegen for aggregation */
/* ------------------------------------------------------------------------ */
#[proc_macro]
pub fn codegen_aggregation(_: TokenStream) -> TokenStream {
    let space = aggregation_space();
    let mut arms = vec![];

    for key_arity in space {
        let arity = key_arity + 1;
        let base_type = Ident::new(&format!("rel_{}", arity), Span::call_site());
        let final_rel = Ident::new(&format!("Collection{}", arity), Span::call_site());

        arms.push(quote! {
            #arity => Rel::#final_rel(
                input_rel.#base_type()
                    .map(row_chop::<#arity, #key_arity, 1>())
                    .reduce_core::<_,ValBuilder<_,_,_,_>,ValSpine<_,_,_,_>>(
                        "aggregation",
                        aggregation_reduce_logic::<#key_arity>(&aggregation)
                    )
                    .as_collection(|k, v| aggregation_merge_kv::<#key_arity, #arity>()((k.clone(), v.clone())))
            )
        });
    }

    let expanded = quote! {
        if input_rel.is_fat() {
            Rel::CollectionFat(
                input_rel.rel_fat()
                    .map(aggregation_separate_kv_fat())
                    .reduce_core::<_,ValBuilder<_,_,_,_>,ValSpine<_,_,_,_>>(
                        "aggregation",
                        aggregation_reduce_logic_fat(&aggregation)
                    )
                    .as_collection(|k, v| aggregation_merge_kv_fat()((k.clone(), v.clone()))),
                idb_catalog.arity()
            )
        } else {
            match idb_catalog.arity() {
                #(#arms),*,
                _ => panic!("codegen_aggregation unimplemented for arity {}", idb_catalog.arity()),
            }
        }
    };
    TokenStream::from(expanded)
}

/* ------------------------------------------------------------------------ */
/* codegen for MIN aggregation optimization with Min semiring */
/* ------------------------------------------------------------------------ */

#[proc_macro]
pub fn codegen_min_optimize(_: TokenStream) -> TokenStream {
    let space = min_optimize_space();
    let mut arms = vec![];

    for arity in space {
        let base_type = Ident::new(&format!("rel_{}", arity), Span::call_site());
        let final_rel = Ident::new(&format!("Collection{}", arity), Span::call_site());
        let key_arity = arity - 1;

        arms.push(quote! {
            #arity => Rel::#final_rel(
                input_rel.#base_type()
                    // Phase 1: Transform input tuples to (key, Min_value) pairs
                    // ========================================================
                    // Extract key columns (all but last) and value column (last)
                    // and carry the value in the Min semiring's difference
                    .inner
                    .flat_map(move |(row, t, _)| {
                        let mut key = reading::row::Row::<#key_arity>::new();
                        for i in 0..#key_arity {
                            key.push(row.column(i));
                        }
                        let value = row.column(#key_arity);
                        std::iter::once((key, reading::Min::new(value))).into_iter().map(move |(x, d2)| (x, t.clone(), d2))
                    })
                    .as_collection()
                    
                    // Phase 2: Apply MIN semiring with thresholding
                    // ========================================================
                    // The threshold_semigroup operator uses MIN semiring semantics:
                    // - Combines multiple values for same key using min() operation
                    // - Only emits updates when minimum actually decreases
                    // - Suppresses redundant updates for non-improving values
                    .threshold_semigroup(|_k, &new_min, current_min| {
                        match current_min {
                            Some(current) if new_min < *current => Some(new_min),
                            Some(_) => None,
                            None if !new_min.is_zero() => Some(new_min),
                            None => None,
                        }
                    })
                    
                    // Phase 3: Convert back to standard tuple representation
                    // =====================================================
                    // Extract minimum value from Min semiring difference
                    // Reconstruct full tuple with key columns + minimum value
                    // Convert to Present semiring for downstream operators
                    .inner
                    .flat_map(move |(key, t, min_val)| {
                        let mut result = reading::row::Row::<#arity>::new();
                        // Add key columns
                        for i in 0..#key_arity {
                            result.push(key.column(i));
                        }
                        // Push minimized value into the last column (extracted from diff!)
                        result.push(min_val.value);
                        std::iter::once((result, reading::semiring_one())).into_iter().map(move |(x2, d2)| (x2, t.clone(), d2))
                    })
                    .as_collection()
            )
        });
    }

    let expanded = quote! {
        if input_rel.is_fat() {
             Rel::CollectionFat(
                input_rel.rel_fat()
                    // Phase 1: Transform fat tuples to (key, Min_value) pairs
                    // =======================================================
                    // Extract key columns (all but last) and value column (last)
                    // and carry the value in the Min semiring's difference
                    .inner
                    .flat_map(move |(row, t, _)| {
                        let mut key = reading::row::FatRow::new();
                        let arity = row.arity();
                            
                        // Extract all columns except the last as the group-by key
                        for i in 0..arity - 1 {
                            key.push(row.column(i));
                        }
                            
                        // Extract the last column as the value to minimize
                        let value = row.column(arity - 1);
                        std::iter::once((key, reading::Min::new(value))).into_iter().map(move |(x, d2)| (x, t.clone(), d2))
                    })
                    .as_collection()
                        
                    // Phase 2: Apply MIN semiring with intelligent thresholding
                    // ========================================================
                    // Same threshold logic as fixed-arity version:
                    // - Only emit updates when minimum actually decreases
                    // - Suppress redundant updates for non-improving values
                    .threshold_semigroup(|_k, &new_min, current_min| {
                        match current_min {
                            Some(current) if new_min < *current => Some(new_min),
                            Some(_) => None,
                            None if !new_min.is_zero() => Some(new_min),
                            None => None,
                        }
                    })
                        
                    // Phase 3: Convert back to complete FatRow representation
                    // ======================================================
                    // Reconstruct full FatRow with key columns + minimum value
                    // Convert to standard semiring for downstream operators
                    .inner
                    .flat_map(move |(key, t, min_val)| {
                        let mut result = reading::row::FatRow::new();
                            
                        // Add all key columns
                        for i in 0..key.arity() {
                            result.push(key.column(i));
                        }
                            
                        // Push minimized value as the last column
                        result.push(min_val.value);
                            
                        std::iter::once((result, reading::semiring_one())).into_iter().map(move |(x2, d2)| (x2, t.clone(), d2))
                    })
                    .as_collection(),
                idb_catalog.arity() // Preserve original arity for fat relation wrapper
            )
        } else {
            match idb_catalog.arity() {
                #(#arms),*,
                _ => panic!("codegen_min_optimize unimplemented for arity {}", idb_catalog.arity()),
            }
        }
    };

    TokenStream::from(expanded)
}



#[cfg(test)]
mod tests {
    use super::*;

    /// Rows and key/value pairs a program can still be holding when it reaches
    /// the fixed-size dispatch, i.e. everything `should_use_fat_mode` does not
    /// send to the fat rows.
    fn rows() -> std::ops::RangeInclusive<usize> {
        1..=ROW_MAX
    }

    fn key_value_halves() -> std::ops::RangeInclusive<usize> {
        1..=KV_MAX
    }

    fn assert_covers<T: PartialEq + std::fmt::Debug>(name: &str, generated: &[T], required: &[T]) {
        let missing = required
            .iter()
            .filter(|shape| !generated.contains(shape))
            .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "{name} has no arm for {} shape(s) a program can present, e.g. {:?}; \
             such a program panics by arity",
            missing.len(),
            &missing[..missing.len().min(4)],
        );
    }

    #[test]
    fn every_dispatch_covers_the_shapes_planned_onto_fixed_size_rows() {
        assert_covers(
            "codegen_row_row / codegen_call_row",
            &row_row_space(),
            &iproduct!(rows(), rows()).collect::<Vec<_>>(),
        );
        assert_covers(
            "codegen_row_kv",
            &row_kv_space(),
            &iproduct!(rows(), key_value_halves(), key_value_halves())
                // A split projects the row, so it cannot widen it.
                .filter(|&(iv, ok, ov)| iv >= ok + ov)
                .collect::<Vec<_>>(),
        );
        assert_covers(
            "codegen_jn",
            &jn_space(),
            &iproduct!(key_value_halves(), key_value_halves(), key_value_halves(), rows())
                // The dispatch site swaps the operands so the wider value is first.
                .filter(|&(_, iv0, iv1, _)| iv0 >= iv1)
                .collect::<Vec<_>>(),
        );
        assert_covers(
            "codegen_kv_k_jn",
            &kv_k_jn_space(),
            &iproduct!(key_value_halves(), key_value_halves(), rows()).collect::<Vec<_>>(),
        );
        assert_covers(
            "codegen_k_k_jn",
            &k_k_jn_space(),
            &iproduct!(key_value_halves(), rows()).collect::<Vec<_>>(),
        );
        assert_covers(
            "codegen_kv_flatten",
            &kv_flatten_space(),
            &iproduct!(key_value_halves(), key_value_halves(), rows())
                // The result projects the key and value together.
                .filter(|&(ik, iv, target)| ik + iv >= target)
                .collect::<Vec<_>>(),
        );
        assert_covers(
            "codegen_k_flatten",
            &k_flatten_space(),
            &iproduct!(key_value_halves(), key_value_halves())
                // The result projects the key.
                .filter(|&(ik, target)| ik >= target)
                .collect::<Vec<_>>(),
        );
        assert_covers(
            "codegen_aggregation",
            &aggregation_space()
                .into_iter()
                .map(|group_by| group_by + 1)
                .collect::<Vec<_>>(),
            &rows().collect::<Vec<_>>(),
        );
        assert_covers(
            "codegen_min_optimize",
            &min_optimize_space(),
            &rows().collect::<Vec<_>>(),
        );
    }

    /// The product table is the one deliberate exception, and it is total by
    /// fallback rather than by enumeration: `codegen_cartesian` routes anything
    /// it does not generate through the fat-row product. This records that the
    /// table really is narrower than the rows, so the fallback is load-bearing
    /// and not dead code.
    #[test]
    fn the_product_table_is_narrower_than_the_rows_and_relies_on_its_fallback() {
        let generated = cartesian_space();
        assert!(
            !generated.contains(&(2, 1, 3)),
            "the product table now covers (2, 1, 3); if it was widened on purpose, \
             this test and the fallback should be revisited together",
        );
        assert!(
            generated.iter().all(|&(iv0, iv1, target)| {
                iv0 <= PROD_MAX && iv1 <= PROD_MAX && target <= PROD_MAX
            }),
            "the product table is bounded by PROD_MAX",
        );
    }
}

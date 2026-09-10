use std::fmt;
// use itertools::Itertools;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::debug;

// use parsing::rule::FLRule;
use crate::collections::CollectionSignature;
use crate::rule::RuleQueryPlan;
use parsing::parser::Program;
use parsing::rule::FLRule;
use crate::strata::GroupStrataQueryPlan;
use catalog::rule::Catalog;
use strata::stratification::Strata;

#[derive(Debug, Clone)]
pub struct ProgramQueryPlan {
    program_plan: Vec<GroupStrataQueryPlan>,
}

impl ProgramQueryPlan {
    pub fn program_plan(&self) -> &Vec<GroupStrataQueryPlan> {
        &self.program_plan
    }

    pub fn new(program_plan: Vec<GroupStrataQueryPlan>) -> Self {
        Self { program_plan }
    }

    pub fn from_strata(strata: &Strata, disable_sharing: bool, opt_level: Option<u8>) -> Self {
        // accumulative seen set across all strata
        let mut seen_set = HashSet::new();
        let program_plan = strata
            .strata()
            .iter()
            .zip(strata.is_recursive_strata_bitmap())
            .flat_map(|(stratum, &is_recursive)| {
                plan_rules(
                    stratum,
                    is_recursive,
                    opt_level,
                    strata.program(),
                    0,
                    &mut seen_set,
                    disable_sharing,
                )
            })
            .collect();

        Self::new(program_plan)
    }

    pub fn max_arity(&self) -> usize {
        self.program_plan
            .iter()
            .flat_map(|group_plan| {
                group_plan
                    .strata_plan()
                    .into_iter()
                    .flat_map(|transformation| {
                        let mut arities = Vec::new();

                        // Get output collection arity
                        let (key_arity, value_arity) = transformation.output().arity();
                        arities.push(key_arity);
                        arities.push(value_arity);

                        // Get input collection(s) arity
                        if transformation.is_unary() {
                            let (key_arity, value_arity) = transformation.unary().arity();
                            arities.push(key_arity);
                            arities.push(value_arity);
                        } else {
                            let (left, right) = transformation.binary();
                            let (left_key, left_value) = left.arity();
                            let (right_key, right_value) = right.arity();
                            arities.push(left_key);
                            arities.push(left_value);
                            arities.push(right_key);
                            arities.push(right_value);
                        }

                        arities
                    })
            })
            .max()
            .unwrap_or(0)
    }

    /// Returns a list of maximal (key_arity, value_arity) tuples that are incomparable.
    /// Two tuples (k1, v1) and (k2, v2) are incomparable if neither dominates the other.
    /// (k1, v1) dominates (k2, v2) if k1 >= k2 AND v1 >= v2, with at least one being a strict inequality.
    pub fn maximal_arity_pairs(&self) -> Vec<(usize, usize)> {
        // Collect all (key_arity, value_arity) pairs from the program
        let mut all_pairs = Vec::new();

        // Collect all pairs
        for group_plan in &self.program_plan {
            for transformation in group_plan.strata_plan() {
                // Get output collection arity
                all_pairs.push(transformation.output().arity());

                // Get input collection(s) arity
                if transformation.is_unary() {
                    all_pairs.push(transformation.unary().arity());
                } else {
                    let (left, right) = transformation.binary();
                    all_pairs.push(left.arity());
                    all_pairs.push(right.arity());
                }
            }
        }

        // Filter out non-maximal pairs and duplicates
        let mut maximal_pairs = Vec::new();

        for pair in &all_pairs {
            if maximal_pairs.contains(pair) {
                continue; // Skip duplicates
            }

            let (k1, v1) = *pair;
            let is_dominated = all_pairs
                .iter()
                .any(|&(k2, v2)| k2 >= k1 && v2 >= v1 && (k2 > k1 || v2 > v1));

            if !is_dominated {
                maximal_pairs.push(*pair);
            }
        }

        maximal_pairs
    }

    /// Determines if fat mode should be used based on the maximum arity required.
    /// Fat mode is REQUIRED for a planned collection the fixed-size
    /// implementations do not cover, since those are generated per arity.
    ///
    /// The two limits are not interchangeable, and reading them as if they were
    /// is how a legal program reached a missing arm. A key-less collection is
    /// one row, so it is bounded by the row limit. A `(key, value)` collection
    /// is *two* rows of the generated key/value tables, and both halves are
    /// bounded by the key/value limit - a `(1, 5)` split has no arm even though
    /// 5 is a perfectly ordinary row width, which is what produced
    /// `codegen_row_kv unimplemented for 6, 1, 5` and
    /// `codegen_jn unimplemented for 1, 5, 1, 6` on programs whose relations
    /// all fit in a row.
    ///
    /// The pairs are the planned collection signatures rather than the declared
    /// relation arities, so an intermediate join signature is covered too; the
    /// maximality filter is sound here because a pair that dominates another
    /// is at least as wide in both components.
    pub fn should_use_fat_mode(
        &self,
        user_requested_fat_mode: bool,
        key_value_limit: usize,
        row_limit: usize,
    ) -> bool {
        let maximal_pairs = self.maximal_arity_pairs();
        let exceeds_fixed_size_rows = maximal_pairs.iter().any(|&(key, value)| {
            if key == 0 {
                value > row_limit
            } else {
                key > key_value_limit || value > key_value_limit
            }
        });
        exceeds_fixed_size_rows || user_requested_fat_mode
    }

    /// Returns detailed arity information for debugging purposes.
    /// Returns a vector of (transformation_name, input_key_value_arities, output_key_value_arity) tuples.
    pub fn arity_analysis(&self) -> Vec<(String, Vec<(usize, usize)>, (usize, usize))> {
        self.program_plan
            .iter()
            .flat_map(|group_plan| {
                group_plan.strata_plan().into_iter().map(|transformation| {
                    let output_arity = transformation.output().arity();

                    let input_arities = if transformation.is_unary() {
                        let arity = transformation.unary().arity();
                        vec![arity]
                    } else {
                        let (left, right) = transformation.binary();
                        let left_arity = left.arity();
                        let right_arity = right.arity();
                        vec![left_arity, right_arity]
                    };

                    let transformation_name =
                        transformation.output().signature().debug_name().to_string();

                    (transformation_name, input_arities, output_arity)
                })
            })
            .collect()
    }
}

impl fmt::Display for ProgramQueryPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, group_plan) in self.program_plan.iter().enumerate() {
            writeln!(f, "#{}\n{}\n", i, group_plan)?;
        }
        Ok(())
    }
}

/// Plan one stratum's rules: the groups the dataflow assembles for them, in order.
///
/// A non-recursive stratum in which any rule takes sideways information passing
/// is sliced into cascading groups, one per expanded plan, because the sliced
/// heads are boundaries the next slice reads.  Any other stratum is one group.
/// Public so that a caller can plan a subset of a stratum -- the state cache
/// plans exactly the units it must recompute.  Sideways slices are named by
/// rule identifier alone, so a caller assembling several subsets into one
/// dataflow passes each a `first_rule_identifier` past the previous subset's.
pub fn plan_rules(
    rules: &[&FLRule],
    is_recursive: bool,
    opt_level: Option<u8>,
    program: &Program,
    first_rule_identifier: usize,
    seen_set: &mut HashSet<Arc<CollectionSignature>>,
    disable_sharing: bool,
) -> Vec<GroupStrataQueryPlan> {
    let embedded_rust = program.embedded_rust();
    let mut rule_identifier = first_rule_identifier;
    let mut any_sip = false;

    let chain: Vec<RuleQueryPlan> = rules
        .iter()
        .flat_map(|&rule| {
            let catalog = Catalog::from_strata(rule);
            let (requested_sip, is_planning) = if catalog
                .is_core_atom_bitmap()
                .into_iter()
                .filter(|&x| *x)
                .count() > 2 { // optimize for <= 2 core atoms are meaningless
                match opt_level {
                    Some(level) => {
                        (level == 1 || level == 3 || rule.is_sip(), level >= 2 || rule.is_planning())
                    }
                    None => (rule.is_sip(), rule.is_planning()),
                }
            } else {
                (false, false)
            };
            // Sideways passing rewrites the relational body; the row program
            // (calls, computed comparisons, head expressions) stays attached to
            // the final rule the rewrite produces.
            let is_sip = requested_sip;

            if is_sip { any_sip = true; } // mark if any rule uses sip in a stratum

            let expanded_catalogs = if is_sip {
                catalog.sideways(rule_identifier)
            } else {
                vec![catalog]
            };
            rule_identifier += 1;

            expanded_catalogs
                .into_iter()
                .map(move |catalog| {
                    RuleQueryPlan::from_catalog_with_embedded(
                        &catalog,
                        is_planning,
                        embedded_rust,
                    )
                })
        })
        .collect();

    // if it is non_recursive and there is some rule using sip, slice into multiple non-recursive strata
    // (because sideways information passing slices the strata into many cascading strata)
    let grouped: Vec<(bool, Vec<RuleQueryPlan>)> = if !is_recursive && any_sip {
        chain.into_iter().map(|plan| (false, vec![plan])).collect() // (to do) this is probably a hacky way to make sideways works
    } else {
        vec![(is_recursive, chain)] // no change to the strata group since it is recursive or no sip rules
    };

    // debugging for each rule plan in the group
    for (is_recursive, rule_plans) in &grouped {
        debug!(
            "-------------------------------- {} strata group --------------------------------",
            if *is_recursive {
                "recursive"
            } else {
                "non-recursive"
            }
        );
        for rule_plan in rule_plans {
            debug!("{}", rule_plan);
        }
    }

    grouped
        .into_iter()
        .map(|(is_recursive, rule_plans)| {
            GroupStrataQueryPlan::new(is_recursive, rule_plans, seen_set, disable_sharing)
        })
        .collect()
}

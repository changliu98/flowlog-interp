use std::fmt;
// use itertools::Itertools;
use std::collections::HashSet;
use tracing::debug;

// use parsing::rule::FLRule;
use crate::rule::RuleQueryPlan;
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
        // Every stratum's rules are numbered past the previous stratum's: the
        // slices planning adds are named by rule number, and every stratum
        // here goes into one dataflow.
        let mut first_rule_identifier = 0;
        let prepared: Vec<PreparedStratum> = strata
            .strata()
            .iter()
            .zip(strata.is_recursive_strata_bitmap())
            .map(|(stratum, &is_recursive)| {
                let first = first_rule_identifier;
                first_rule_identifier += stratum.len();
                prepare_rules(stratum, is_recursive, opt_level, first)
            })
            .collect();
        // Planning one rule reads nothing but the rule: every rule of every
        // stratum is planned at once. Sharing is decided afterwards, in
        // program order, by the accumulated seen set.
        let jobs: Vec<&PlanJob> = prepared
            .iter()
            .flat_map(|stratum| stratum.slices.iter().flatten().chain(stratum.rules.iter()))
            .collect();
        let embedded_rust = strata.program().embedded_rust();
        let mut planned = parallel_map(&jobs, |job| job.plan(embedded_rust)).into_iter();

        // accumulative seen set across all strata
        let mut seen_set = HashSet::new();
        let program_plan = prepared
            .iter()
            .flat_map(|stratum| {
                let mut groups = Vec::new();
                for slices in &stratum.slices {
                    let plans: Vec<RuleQueryPlan> = slices
                        .iter()
                        .flat_map(|_| planned.next().expect("one outcome per job").plans)
                        .collect();
                    groups.push(GroupStrataQueryPlan::new(false, plans, &mut seen_set, disable_sharing));
                }
                let mut any_sip = false;
                let mut chain: Vec<RuleQueryPlan> = Vec::new();
                for _ in &stratum.rules {
                    let outcome = planned.next().expect("one outcome per job");
                    any_sip |= outcome.sip;
                    chain.extend(outcome.plans);
                }
                // if it is non_recursive and there is some rule using sip, slice into multiple non-recursive strata
                // (because sideways information passing slices the strata into many cascading strata)
                let grouped: Vec<(bool, Vec<RuleQueryPlan>)> = if !stratum.is_recursive && any_sip {
                    chain.into_iter().map(|plan| (false, vec![plan])).collect() // (to do) this is probably a hacky way to make sideways works
                } else {
                    vec![(stratum.is_recursive, chain)] // no change to the strata group since it is recursive or no sip rules
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
                groups.extend(grouped.into_iter().map(|(is_recursive, rule_plans)| {
                    GroupStrataQueryPlan::new(is_recursive, rule_plans, &mut seen_set, disable_sharing)
                }));
                groups
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

#[cfg(test)]
mod tests {
    use super::*;
    use parsing::parser::Program;

    #[test]
    fn slices_of_rules_in_different_strata_have_different_names() {
        // The two rules of h land in different strata, at the same place in
        // each; both are cut, and one dataflow holds both slices.
        let source = ".code rust\npub fn lt(a: i64, b: i64) -> bool { a < b }\n.endcode\n\
                      .in\n.decl e(a: number, b: number)\n.input e.facts\n\
                      .decl f(a: number, b: number)\n.input f.facts\n\
                      .decl g(a: number, b: number)\n.input g.facts\n\
                      .printsize\n.decl h(a: number, b: number)\n.decl k(a: number, b: number)\n\
                      .rule\n\
                      h(X, W) :- k(X, Y), f(Y, Z), g(Z, W), @call(lt, X, Z).\n\
                      k(X, Y) :- f(X, Y).\n\
                      h(X, W) :- e(X, Y), f(Y, Z), e(Z, W), @call(lt, X, Z).\n";
        let program = Program::parse(source, "slices.dl").expect("parses");
        let strata = Strata::from_parser(program);
        let plan = ProgramQueryPlan::from_strata(&strata, false, None);
        let mut slices: Vec<String> = plan
            .program_plan()
            .iter()
            .flat_map(|group| group.rules().iter().map(|rule| rule.head().name().to_string()))
            .filter(|name| name.starts_with("__flowlog_pushdown__"))
            .collect();
        assert_eq!(slices.len(), 2, "{slices:?}");
        slices.dedup();
        assert_eq!(slices.len(), 2, "{slices:?}");
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

/// One rule to plan: the rule as it is planned (a pushdown slice, a rest, or
/// the rule as written), its identifier, and how to plan it.
struct PlanJob {
    rule: FLRule,
    /// The rule's number, which names the slices sideways passing adds.
    identifier: usize,
    /// A pushdown slice is planned as written: never sideways.
    slice: bool,
    opt_level: Option<u8>,
}

/// What planning one job produced: the rule's plans (several under sideways
/// passing) and whether it passed sideways.
struct PlannedJob {
    plans: Vec<RuleQueryPlan>,
    sip: bool,
}

impl PlanJob {
    fn plan(&self, embedded_rust: Option<&parsing::embedded::EmbeddedRust>) -> PlannedJob {
        let catalog = Catalog::from_strata(&self.rule);
        // optimize for <= 2 core atoms are meaningless
        let many_cores = catalog.is_core_atom_bitmap().iter().filter(|&&is_core| is_core).count() > 2;
        if self.slice {
            let planning = many_cores
                && (self.opt_level.is_some_and(|level| level >= 2) || self.rule.is_planning());
            return PlannedJob {
                plans: vec![RuleQueryPlan::from_catalog_with_embedded(&catalog, planning, embedded_rust)],
                sip: false,
            };
        }
        let (requested_sip, is_planning) = if many_cores {
            match self.opt_level {
                Some(level) => {
                    (level == 1 || level == 3 || self.rule.is_sip(), level >= 2 || self.rule.is_planning())
                }
                None => (self.rule.is_sip(), self.rule.is_planning()),
            }
        } else {
            (false, false)
        };
        // Sideways passing rewrites the relational body; the row program
        // (calls, computed comparisons, head expressions) stays attached to
        // the final rule the rewrite produces.
        let is_sip = requested_sip;
        let expanded_catalogs = if is_sip {
            catalog.sideways(self.identifier)
        } else {
            vec![catalog]
        };
        PlannedJob {
            plans: expanded_catalogs
                .iter()
                .map(|catalog| RuleQueryPlan::from_catalog_with_embedded(catalog, is_planning, embedded_rust))
                .collect(),
            sip: is_sip,
        }
    }
}

/// One stratum's rules, ready to plan: the pushdown slices by level, then the
/// rules themselves.
struct PreparedStratum {
    is_recursive: bool,
    slices: Vec<Vec<PlanJob>>,
    rules: Vec<PlanJob>,
}

/// Decide what to plan for one stratum. Filter calls run where their
/// arguments are bound (see `pushdown`). The slices of one level are planned
/// as one group ahead of the stratum's rules, so the rules of every head stay
/// in one group and a slice is complete before anything reads it.
fn prepare_rules(
    rules: &[&FLRule],
    is_recursive: bool,
    opt_level: Option<u8>,
    first_rule_identifier: usize,
) -> PreparedStratum {
    let mut slices: Vec<Vec<PlanJob>> = Vec::new();
    let mut planned_rules = Vec::with_capacity(rules.len());
    // Only where the plan joins core atoms in the order the rule lists them:
    // not under sideways passing or the optimizer.
    let pushdown = !is_recursive && matches!(opt_level, None | Some(0));
    for (offset, &rule) in rules.iter().enumerate() {
        let identifier = first_rule_identifier + offset;
        let cut = (pushdown && !rule.is_sip() && !rule.is_planning())
            .then(|| crate::pushdown::push_down_filter_calls(rule, identifier))
            .flatten();
        let planned = match cut {
            Some((rule_slices, rest)) => {
                for (level, slice) in rule_slices.into_iter().enumerate() {
                    if slices.len() <= level {
                        slices.push(Vec::new());
                    }
                    slices[level].push(PlanJob { rule: slice, identifier, slice: true, opt_level });
                }
                rest
            }
            None => rule.clone(),
        };
        planned_rules.push(PlanJob { rule: planned, identifier, slice: false, opt_level });
    }
    PreparedStratum { is_recursive, slices, rules: planned_rules }
}

/// `function` over every item, on a few threads when there are enough
/// items for it to pay, results in item order.
fn parallel_map<T: Sync, R: Send>(items: &[T], function: impl Fn(&T) -> R + Sync) -> Vec<R> {
    const PER_THREAD: usize = 32;
    let threads = std::thread::available_parallelism()
        .map_or(1, |count| count.get())
        .min(8)
        .min(items.len() / PER_THREAD);
    if threads <= 1 {
        return items.iter().map(function).collect();
    }
    let chunk = items.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|part| {
                let function = &function;
                scope.spawn(move || part.iter().map(function).collect::<Vec<R>>())
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
            .collect()
    })
}

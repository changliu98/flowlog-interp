//! Relation states and the counters of one run.
//!
//! A `RelationState` is one relation's rows at the boundary of an evaluation:
//! an input as it enters the dataflow, or a head as it settled. Its rows are
//! sorted and distinct, so two states of one set hold the same rows in the
//! same order whatever order the engine emitted them in.

use crate::rows::Rows;
use parsing::Val;
use serde::Serialize;
use std::sync::Arc;

/// One relation's rows, sorted and distinct.
#[derive(Debug)]
pub struct RelationState {
    pub name: String,
    pub arity: usize,
    pub rows: Arc<Rows>,
}

impl RelationState {
    pub fn new(name: &str, arity: usize, mut rows: Vec<Vec<Val>>) -> Self {
        rows.sort_unstable();
        rows.dedup();
        Self::from_sorted(name, arity, Rows::from_vecs(arity, &rows))
    }

    /// A sorted, distinct boundary produced by the input or capture merge.
    pub fn from_sorted(name: &str, arity: usize, rows: Rows) -> Self {
        debug_assert!(rows.is_sorted_set());
        debug_assert_eq!(rows.arity(), arity);
        Self {
            name: name.to_string(),
            arity,
            rows: Arc::new(rows),
        }
    }

    pub fn empty(name: &str, arity: usize) -> Self {
        Self::from_sorted(name, arity, Rows::new(arity))
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// What one evaluation did and what it cost.
///
/// - `strata`, `rules`: strata of the program; source rules evaluated (before
///   any sideways expansion).
/// - `planning_micros`: parsing through planning, compiling embedded blocks,
///   starting the workers; `execution_micros`: the dataflow, to its last
///   update; `settle_micros`: merging the captured heads into states;
///   `total_micros`: the whole run.
/// - `peak_memory_bytes`: bytes the evaluation's threads held at most, as the
///   allocator saw them; `materialized_rows`: rows of the settled heads.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RunStats {
    pub strata: usize,
    pub rules: usize,
    pub planning_micros: u64,
    pub execution_micros: u64,
    pub settle_micros: u64,
    pub total_micros: u64,
    pub peak_memory_bytes: i64,
    pub materialized_rows: u64,
}

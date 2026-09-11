//! The aggregation kernels: split a head row into its group-by key and the
//! aggregated column, reduce each group, and put the result back where the
//! aggregate sits in the head.
//!
//! The aggregate may occupy any column of the head. The key is the other
//! columns in their head order, so `R(k, min(v), j)` groups by `(k, j)` and
//! writes the minimum back into the middle column.

use parsing::aggregation::{Aggregation, AggregationOperator};
use reading::row::{Array, FatRow, Row};
use reading::{semiring_one, Semiring, Val};

/// Aggregates a group's values.
///
/// `count` counts the rows of the group (which are distinct, the input being
/// a set); `sum` wraps on overflow like every other arithmetic of the engine.
fn aggregate_ints(input: impl Iterator<Item = Val>, op: &AggregationOperator) -> Option<Val> {
    match op {
        AggregationOperator::Count => Some(input.count() as Val),
        AggregationOperator::Sum => Some(input.fold(0 as Val, |sum, value| sum.wrapping_add(value))),
        AggregationOperator::Min => input.min(),
        AggregationOperator::Max => input.max(),
    }
}

/// The reduction logic differential dataflow applies per group.
///
/// `reduce_core` calls this with four arguments: the key, the key's input
/// values, the output this key already carries (which the operator clears
/// afterwards), and the updates to emit. The result therefore belongs in the
/// fourth argument; anything pushed into the third is discarded.
pub fn aggregation_reduce_logic<const N_GB: usize>(
    aggregation: &Aggregation,
) -> impl FnMut(
    &Row<N_GB>,
    &[(&Row<1>, Semiring)],
    &mut Vec<(Row<1>, Semiring)>,
    &mut Vec<(Row<1>, Semiring)>,
) {
    let operator = aggregation.operator().clone();

    move |_key, input, _existing_output, updates| {
        let mut out = Row::<1>::builder();
        let values = input.iter().map(|(row, _)| row.column(0));
        if let Some(result) = aggregate_ints(values, &operator) {
            out.push(result);
            updates.push((out.finish(), semiring_one()));
        }
    }
}

/// Splits a head row into its group-by key (every column but `position`, in
/// order) and the aggregated column.
pub fn aggregation_separate<const ARITY: usize, const KEY: usize>(
    position: usize,
) -> impl Fn(Row<ARITY>) -> (Row<KEY>, Row<1>) {
    move |row| {
        let mut key = Row::<KEY>::builder();
        let mut value = Row::<1>::builder();
        for column in 0..ARITY {
            if column == position {
                value.push(row.column(column));
            } else {
                key.push(row.column(column));
            }
        }
        (key.finish(), value.finish())
    }
}

/// Rebuilds a head row from its group-by key and the aggregate, which goes
/// back into `position`.
pub fn aggregation_merge<const KEY: usize, const ARITY: usize>(
    position: usize,
) -> impl Fn((Row<KEY>, Row<1>)) -> Row<ARITY> {
    move |(key, value)| {
        let mut out = Row::<ARITY>::builder();
        let mut next_key = 0;
        for column in 0..ARITY {
            if column == position {
                out.push(value.column(0));
            } else {
                out.push(key.column(next_key));
                next_key += 1;
            }
        }
        out.finish()
    }
}

// ============================================================================
// Fat Row Variants
// ============================================================================

/// Fat row version of aggregation reduce logic.
pub fn aggregation_reduce_logic_fat(
    aggregation: &Aggregation,
) -> impl FnMut(
    &FatRow,
    &[(&Row<1>, Semiring)],
    &mut Vec<(Row<1>, Semiring)>,
    &mut Vec<(Row<1>, Semiring)>,
) {
    let operator = aggregation.operator().clone();

    move |_key, input, _existing_output, updates| {
        let mut out = Row::<1>::builder();
        let values = input.iter().map(|(row, _)| row.column(0));
        if let Some(result) = aggregate_ints(values, &operator) {
            out.push(result);
            updates.push((out.finish(), semiring_one()));
        }
    }
}

/// Fat row version of `aggregation_separate`.
pub fn aggregation_separate_fat(position: usize) -> impl Fn(FatRow) -> (FatRow, Row<1>) {
    move |row| {
        let mut key = FatRow::new();
        let mut value = Row::<1>::builder();
        for column in 0..row.arity() {
            if column == position {
                value.push(row.column(column));
            } else {
                key.push(row.column(column));
            }
        }
        (key, value.finish())
    }
}

/// Fat row version of `aggregation_merge`.
pub fn aggregation_merge_fat(position: usize) -> impl Fn((FatRow, Row<1>)) -> FatRow {
    move |(key, value)| {
        let mut out = FatRow::new();
        let arity = key.arity() + 1;
        let mut next_key = 0;
        for column in 0..arity {
            if column == position {
                out.push(value.column(0));
            } else {
                out.push(key.column(next_key));
                next_key += 1;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_aggregate_column_round_trips_from_any_position() {
        let mut row = Row::<3>::builder();
        row.push(10);
        row.push(20);
        row.push(30);
        let row = row.finish();
        let (key, value) = aggregation_separate::<3, 2>(1)(row.clone());
        assert_eq!((key.column(0), key.column(1), value.column(0)), (10, 30, 20));
        let merged = aggregation_merge::<2, 3>(1)((key, value));
        assert_eq!(merged, row);

        let mut fat = FatRow::new();
        fat.push(1);
        fat.push(2);
        let (key, value) = aggregation_separate_fat(0)(fat.clone());
        assert_eq!((key.arity(), value.column(0)), (1, 1));
        assert_eq!(aggregation_merge_fat(0)((key, value)), fat);
    }
}

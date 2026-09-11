//! Merge sorted, locally consolidated worker runs without a serial heap walk.

use crate::accounting::{MemoryCounter, MemoryTag};
use std::cmp::Ordering;
use std::sync::Arc;

// Small boundaries cost less to merge on the caller than to start threads.
const PARALLEL_ROWS: usize = 131_072;

/// Each input run must be sorted and contain at most one item per key.
/// `combine` folds equal keys and returns whether the resulting item is kept.
/// The merge tree uses at most one active thread per input run. Spawned
/// materialization threads carry the evaluation's memory accounting tag.
pub(crate) fn sorted<T, C, F>(
    mut runs: Vec<Vec<T>>,
    compare: &C,
    combine: &F,
    memory: Option<&Arc<MemoryCounter>>,
) -> Vec<T>
where
    T: Send,
    C: Fn(&T, &T) -> Ordering + Sync,
    F: Fn(&mut T, T) -> bool + Sync,
{
    if runs.len() <= 1 {
        return runs.pop().unwrap_or_default();
    }
    let rows: usize = runs.iter().map(Vec::len).sum();
    let right = runs.split_off(runs.len() / 2);
    let (left, right) = if rows >= PARALLEL_ROWS {
        std::thread::scope(|scope| {
            let right = scope.spawn(move || {
                let _tag = memory.map(MemoryTag::new);
                sorted(right, compare, combine, memory)
            });
            let left = sorted(runs, compare, combine, memory);
            let right = right.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            (left, right)
        })
    } else {
        (sorted(runs, compare, combine, memory), sorted(right, compare, combine, memory))
    };
    pair(left, right, compare, combine)
}

fn pair<T, C, F>(mut left: Vec<T>, mut right: Vec<T>, compare: &C, combine: &F) -> Vec<T>
where
    C: Fn(&T, &T) -> Ordering,
    F: Fn(&mut T, T) -> bool,
{
    if left.is_empty() { return right; }
    if right.is_empty() { return left; }
    if compare(left.last().unwrap(), &right[0]) == Ordering::Less {
        left.append(&mut right);
        return left;
    }
    if compare(right.last().unwrap(), &left[0]) == Ordering::Less {
        right.append(&mut left);
        return right;
    }
    let mut result = Vec::with_capacity(left.len() + right.len());
    let mut left = left.into_iter().peekable();
    let mut right = right.into_iter().peekable();
    while let (Some(a), Some(b)) = (left.peek(), right.peek()) {
        match compare(a, b) {
            Ordering::Less => result.push(left.next().unwrap()),
            Ordering::Greater => result.push(right.next().unwrap()),
            Ordering::Equal => {
                let mut item = left.next().unwrap();
                if combine(&mut item, right.next().unwrap()) {
                    result.push(item);
                }
            }
        }
    }
    result.extend(left);
    result.extend(right);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn parallel_merge_matches_a_signed_reference_with_overlapping_runs() {
        let mut expected = BTreeMap::<i64, i64>::new();
        let runs = (0..32).map(|worker| {
            (0..5000).map(|index| {
                let key = index * 17 + worker % 17;
                let weight = if worker % 3 == 0 { -2 } else { 1 };
                *expected.entry(key).or_default() += weight;
                (key, weight)
            }).collect()
        }).collect();
        let actual = sorted(runs, &|a: &(i64, i64), b| a.0.cmp(&b.0), &|a, b| {
            a.1 += b.1;
            a.1 != 0
        }, None);
        expected.retain(|_, weight| *weight != 0);
        assert_eq!(actual, expected.into_iter().collect::<Vec<_>>());
    }
}

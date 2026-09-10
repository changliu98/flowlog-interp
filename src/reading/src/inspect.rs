/* -----------------------------------------------------------------------------------------------
 * inspection and capture
 * -----------------------------------------------------------------------------------------------
 */
use differential_dataflow::collection::{AsCollection, VecCollection};
use differential_dataflow::lattice::Lattice;
use differential_dataflow::{ExchangeData, Hashable};
use std::sync::{Arc, Mutex};
use timely::dataflow::operators::probe::Handle as ProbeHandle;
use timely::dataflow::operators::{Inspect, Map, Probe};
use timely::dataflow::Scope;
use timely::order::TotalOrder;

use crate::rel::{dedup_retained_collection, Rel};
use crate::row::Array;
use crate::{semiring_weight, Semiring, Val};
use tracing::{debug, info};

/// Prints the size of a relation (number of tuples)
fn printsize<G, D>(rel: &VecCollection<G, D, Semiring>, name: &str, is_recursive: bool)
where
    G: Scope,
    G::Timestamp: Lattice + TotalOrder,
    D: ExchangeData + Hashable,
{
    let prefix = if is_recursive {
        format!("Delta of (recursive) {}", name)
    } else {
        format!("Size of (non-recursive) {}", name)
    };

    dedup_retained_collection(rel)
        .inner
        .flat_map(move |(_, t, _)| {
            Some(((), 1_i32))
                .into_iter()
                .map(move |(x, d2)| (x, t.clone(), d2))
        })
        .as_collection()
        .map(|_| ())
        .consolidate()
        .inspect(move |x| info!("{}: {:?}", prefix, x));
}

/// Prints the content of a relation (all tuples)
fn print<G, D>(rel: &VecCollection<G, D, Semiring>, name: &str)
where
    G: Scope,
    G::Timestamp: Lattice + TotalOrder,
    D: ExchangeData + Hashable + std::fmt::Display,
{
    let name = name.to_owned();
    dedup_retained_collection(rel)
        .inner
        .flat_map(move |(x, t, _)| {
            Some((x, 1_i32))
                .into_iter()
                .map(move |(x, d2)| (x, t.clone(), d2))
        })
        .as_collection()
        .inspect(move |(data, time, delta)| debug!("{}: ({}, {:?}, {})", name, data, time, delta));
    // use std::fmt::Display for D (i.e. Row)
}

/// Updates captured while materializing a relation at a cache boundary.
///
/// The integer difference lets the same capture path work in both the default
/// `Present` build and the `isize` build. Callers consolidate the updates after
/// the worker frontier has completed.
pub type MaterializedUpdates = Arc<Mutex<Vec<(Vec<Val>, isize)>>>;

fn capture<G, D>(
    rel: &VecCollection<G, D, Semiring>,
    updates: MaterializedUpdates,
    probe: &ProbeHandle<G::Timestamp>,
) where
    G: Scope,
    G::Timestamp: Lattice + TotalOrder,
    D: ExchangeData + Hashable + Array,
{
    dedup_retained_collection(rel)
        .inner
        .probe_with(probe)
        .inspect(move |(row, _time, difference)| {
            let values = (0..row.arity())
                .map(|column| row.column(column))
                .collect::<Vec<_>>();
            updates
                .lock()
                .expect("materialized relation lock poisoned")
                .push((values, semiring_weight(difference)));
        });
}

/// Materialize a type-erased relation into a process-owned update buffer,
/// reporting completion through `probe`.
pub fn capture_generic<G>(rel: &Rel<G>, updates: MaterializedUpdates, probe: &ProbeHandle<G::Timestamp>)
where
    G: Scope,
    G::Timestamp: Lattice + TotalOrder,
{
    if rel.is_fat() {
        capture(rel.rel_fat(), updates, probe);
    } else {
        match rel.arity() {
            0 => capture(rel.rel_0(), updates, probe),
            1 => capture(rel.rel_1(), updates, probe),
            2 => capture(rel.rel_2(), updates, probe),
            3 => capture(rel.rel_3(), updates, probe),
            4 => capture(rel.rel_4(), updates, probe),
            5 => capture(rel.rel_5(), updates, probe),
            6 => capture(rel.rel_6(), updates, probe),
            7 => capture(rel.rel_7(), updates, probe),
            8 => capture(rel.rel_8(), updates, probe),
            arity => unreachable!("arity {arity} should be handled by fixed-size capture variants"),
        }
    }
}

/// Prints the content of a relation with any arity
pub fn print_generic<G>(rel: &Rel<G>, name: &str)
where
    G: Scope,
    G::Timestamp: Lattice + TotalOrder,
{
    if rel.is_fat() {
        print(rel.rel_fat(), name)
    } else {
        let arity = rel.arity();
        match arity {
            0 => print(rel.rel_0(), name),
            1 => print(rel.rel_1(), name),
            2 => print(rel.rel_2(), name),
            3 => print(rel.rel_3(), name),
            4 => print(rel.rel_4(), name),
            5 => print(rel.rel_5(), name),
            6 => print(rel.rel_6(), name),
            7 => print(rel.rel_7(), name),
            8 => print(rel.rel_8(), name),
            _ => unreachable!("arity {} should be handled by fixed-size variants", arity),
        }
    }
}

/// Prints the size of a relation with any arity
pub fn printsize_generic<G>(rel: &Rel<G>, name: &str, is_recursive: bool)
where
    G: Scope,
    G::Timestamp: Lattice + TotalOrder,
{
    if rel.is_fat() {
        printsize(rel.rel_fat(), name, is_recursive)
    } else {
        let arity = rel.arity();
        match arity {
            0 => printsize(rel.rel_0(), name, is_recursive),
            1 => printsize(rel.rel_1(), name, is_recursive),
            2 => printsize(rel.rel_2(), name, is_recursive),
            3 => printsize(rel.rel_3(), name, is_recursive),
            4 => printsize(rel.rel_4(), name, is_recursive),
            5 => printsize(rel.rel_5(), name, is_recursive),
            6 => printsize(rel.rel_6(), name, is_recursive),
            7 => printsize(rel.rel_7(), name, is_recursive),
            8 => printsize(rel.rel_8(), name, is_recursive),
            _ => unreachable!("arity {} should be handled by fixed-size variants", arity),
        }
    }
}


use timely::progress::Timestamp;
/* -----------------------------------------------------------------------------------------------
 * inspection and capture
 * -----------------------------------------------------------------------------------------------
 */
use differential_dataflow::collection::{AsCollection, VecCollection};
use differential_dataflow::difference::{IsZero, Semigroup};
use differential_dataflow::lattice::Lattice;
use differential_dataflow::{ExchangeData, Hashable};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use timely::dataflow::operators::probe::Handle as ProbeHandle;
use timely::dataflow::operators::{Inspect, Probe};
use timely::dataflow::operators::vec::Map;
use timely::order::TotalOrder;

use crate::rel::{dedup_retained_collection, Rel};
use crate::row::Array;
use crate::{semiring_weight, Semiring, Val};
use tracing::{debug, info};

/// Prints the size of a relation (number of tuples)
fn printsize<'scope, T: Timestamp, D>(rel: VecCollection<'scope, T, D, Semiring>, name: &str, is_recursive: bool)
where
    T: Lattice + TotalOrder,
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
fn print<'scope, T: Timestamp, D>(rel: VecCollection<'scope, T, D, Semiring>, name: &str)
where
    T: Lattice + TotalOrder,
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
/// `Present` build and the `isize` build.
pub type UpdateBatch = Vec<(Vec<Val>, isize)>;

/// Sorted worker batches, published after their capture frontiers complete.
/// Each row occurs at most once per batch; signed weights can still cancel
/// across batches. Flushing holds the shared lock for one vector push.
pub type MaterializedUpdates = Arc<Mutex<Vec<UpdateBatch>>>;

/// A worker's capture buffer. It stays on that worker until completion; no
/// process-shared lock is acquired while records arrive.
pub struct WorkerCapture {
    flush: Box<dyn Fn()>,
}

impl WorkerCapture {
    /// Publish the whole buffer after the downstream probe has completed.
    pub fn flush(&self) {
        (self.flush)();
    }
}

fn capture<'scope, T: Timestamp, D>(
    rel: VecCollection<'scope, T, D, Semiring>,
    updates: MaterializedUpdates,
    probe: &ProbeHandle<T>,
) -> WorkerCapture
where
    T: Lattice + TotalOrder,
    D: ExchangeData + Hashable + Array,
{
    // Keep the native row inline while collecting and sorting. Convert to
    // the public Vec<Val> representation only once per retained worker row.
    let local = Rc::new(RefCell::new(Vec::<(D, Semiring)>::new()));
    let captured = Rc::clone(&local);
    // This is a final snapshot, so normalize signed weights at materialization
    // instead of adding another exchange/arrangement/threshold to the dataflow.
    rel.inner
        .inspect(move |(row, _time, difference)| {
            captured.borrow_mut().push((row.clone(), *difference));
        })
        .probe_with(probe);
    WorkerCapture { flush: Box::new(move || {
        let mut rows = std::mem::take(&mut *local.borrow_mut());
        rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        rows.dedup_by(|current, previous| {
            if current.0 == previous.0 {
                previous.1.plus_equals(&current.1);
                true
            } else {
                false
            }
        });
        let batch: UpdateBatch = rows.into_iter()
            .filter(|(_, difference)| !difference.is_zero())
            .map(|(row, difference)| {
                let values = (0..row.arity()).map(|column| row.column(column)).collect();
                (values, semiring_weight(&difference))
            })
            .collect();
        if !batch.is_empty() {
            updates.lock().expect("materialized relation lock poisoned").push(batch);
        }
    }) }
}

/// Capture a type-erased relation locally. The caller must retain the returned
/// buffer and flush it after `probe` completes, before reporting worker success.
pub fn capture_generic<'scope, T: Timestamp>(rel: &Rel<'scope, T>, updates: MaterializedUpdates, probe: &ProbeHandle<T>) -> WorkerCapture
where
    T: Lattice + TotalOrder,
{
    if rel.is_fat() {
        capture(rel.rel_fat(), updates, probe)
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
pub fn print_generic<'scope, T: Timestamp>(rel: &Rel<'scope, T>, name: &str)
where
    T: Lattice + TotalOrder,
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
pub fn printsize_generic<'scope, T: Timestamp>(rel: &Rel<'scope, T>, name: &str, is_recursive: bool)
where
    T: Lattice + TotalOrder,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row::Row;
    use crate::semiring_one;
    use differential_dataflow::input::Input;
    use std::collections::BTreeMap;

    #[test]
    fn workers_publish_complete_batches_with_signed_updates() {
        let updates = MaterializedUpdates::default();
        let shared = Arc::clone(&updates);
        timely::execute(timely::Config::process(4), move |worker| {
            let probe = ProbeHandle::new();
            let (mut input, capture) = worker.dataflow::<u64, _, _>(|scope| {
                let (input, collection) = scope.new_collection::<Row<1>, Semiring>();
                let capture = capture_generic(&Rel::Collection1(collection), Arc::clone(&shared), &probe);
                (input, capture)
            });
            let row = |value| {
                let mut row = Row::<1>::builder();
                row.push(value);
                row.finish()
            };
            for value in (worker.index()..1024).step_by(worker.peers()) {
                input.update(row(value as Val), semiring_one());
                input.update(row(value as Val), semiring_one());
            }
            #[cfg(feature = "isize-type")]
            if worker.index() == 0 {
                input.update(row(-1), 1);
            }
            input.advance_to(1);
            input.flush();
            worker.step_while(|| probe.less_than(input.time()));
            assert!(shared.lock().unwrap().is_empty(), "capture published before flush");
            #[cfg(feature = "isize-type")]
            for value in (worker.index()..1024).step_by(worker.peers()).filter(|v| v % 3 == 0) {
                input.update(row(value as Val), -2);
            }
            #[cfg(feature = "isize-type")]
            if worker.index() == 1 {
                input.update(row(-1), -1);
            }
            input.close();
            worker.step_while(|| !probe.done());
            capture.flush();
        }).unwrap().join().into_iter().for_each(|result| result.unwrap());
        let batches = updates.lock().unwrap();
        assert_eq!(batches.len(), 4);
        let mut totals = BTreeMap::<Val, isize>::new();
        for (row, weight) in batches.iter().flatten() {
            *totals.entry(row[0]).or_default() += weight;
        }
        #[cfg(feature = "isize-type")]
        {
            assert!(batches.iter().flatten().any(|(_, weight)| *weight < 0));
            assert_eq!(totals.get(&-1), Some(&0), "weights must cancel across workers");
        }
        for value in 0..1024 {
            let present = !(cfg!(feature = "isize-type") && value % 3 == 0);
            assert_eq!(totals.get(&value).is_some_and(|weight| *weight > 0), present);
        }
    }
}

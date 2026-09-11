//! Per-evaluation accounting: the time an evaluation has left, whether it was
//! asked to stop, the bytes its threads have allocated, the rows it has
//! materialized, and the first fault any of its operators met.
//!
//! An evaluation runs on threads of its own (see `worker`), so bytes can be
//! attributed to it by tagging those threads: the global allocator adds every
//! allocation and subtraction on a tagged thread to that evaluation's counter.
//! Frees of memory allocated elsewhere are charged to the freeing thread, so
//! the figure is an estimate that is exact when an evaluation frees what it
//! allocated, which is what an evaluation does.
//!
//! Every operator closure the engine generates holds the evaluation's
//! `Budget` and asks `stopped()` before it emits: once a deadline passes, a
//! ceiling is crossed, a cancellation arrives or an operator faults, every
//! operator falls silent, the dataflow drains, and the evaluation returns
//! the reason. A function that never returns cannot be stopped this way; the
//! budget is checked between operators, not inside them.

use parsing::diagnostic::{Diagnostic, Result};
use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ memory

/// Bytes currently held and the most held at once, for one evaluation.
#[derive(Debug, Default)]
pub struct MemoryCounter {
    current: AtomicI64,
    peak: AtomicI64,
}

impl MemoryCounter {
    fn apply(&self, delta: i64) {
        let now = self.current.fetch_add(delta, Ordering::Relaxed) + delta;
        if now > self.peak.load(Ordering::Relaxed) {
            self.peak.fetch_max(now, Ordering::Relaxed);
        }
    }

    pub fn current(&self) -> i64 {
        self.current.load(Ordering::Relaxed)
    }

    pub fn peak(&self) -> i64 {
        self.peak.load(Ordering::Relaxed)
    }
}

thread_local! {
    static COUNTER: Cell<*const MemoryCounter> = const { Cell::new(std::ptr::null()) };
    static PENDING: Cell<i64> = const { Cell::new(0) };
}

/// Bytes a thread accumulates before touching the shared counter.
const FLUSH_BYTES: i64 = 1 << 16;

#[inline]
fn account(delta: i64) {
    let _ = PENDING.try_with(|pending| {
        let total = pending.get() + delta;
        pending.set(total);
        if total.abs() >= FLUSH_BYTES {
            flush_pending(pending);
        }
    });
}

#[inline]
fn flush_pending(pending: &Cell<i64>) {
    let total = pending.replace(0);
    let _ = COUNTER.try_with(|counter| {
        let pointer = counter.get();
        if !pointer.is_null() {
            // SAFETY: a tagged thread's counter is kept alive by the tag guard
            // that set it, which clears the pointer before it is dropped.
            unsafe { (*pointer).apply(total) }
        }
    });
}

/// The allocator that attributes bytes to the evaluation a thread serves.
pub struct CountingAllocator<A>(pub A);

unsafe impl<A: GlobalAlloc> GlobalAlloc for CountingAllocator<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = self.0.alloc(layout);
        if !pointer.is_null() {
            account(layout.size() as i64);
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = self.0.alloc_zeroed(layout);
        if !pointer.is_null() {
            account(layout.size() as i64);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        self.0.dealloc(pointer, layout);
        account(-(layout.size() as i64));
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let moved = self.0.realloc(pointer, layout, new_size);
        if !moved.is_null() {
            account(new_size as i64 - layout.size() as i64);
        }
        moved
    }
}

/// While alive, the current thread's allocations are charged to a counter.
pub struct MemoryTag {
    _counter: Arc<MemoryCounter>,
}

impl MemoryTag {
    pub fn new(counter: &Arc<MemoryCounter>) -> Self {
        let _ = PENDING.try_with(|pending| pending.set(0));
        let _ = COUNTER.try_with(|current| current.set(Arc::as_ptr(counter)));
        Self {
            _counter: Arc::clone(counter),
        }
    }
}

impl Drop for MemoryTag {
    fn drop(&mut self) {
        let _ = PENDING.try_with(flush_pending);
        let _ = COUNTER.try_with(|current| current.set(std::ptr::null()));
    }
}

// ------------------------------------------------------------------ budget

/// A handle a caller keeps to stop an evaluation from another thread.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// The limits one evaluation runs under.
#[derive(Debug, Clone, Default)]
pub struct Limits {
    /// Wall-clock time the evaluation may take.
    pub time: Option<Duration>,
    /// A token the caller may cancel.
    pub cancel: Option<CancelToken>,
    /// Bytes the evaluation's threads may hold at once.
    pub memory_bytes: Option<u64>,
    /// Rows the evaluation may materialize at its unit boundaries.
    pub tuples: Option<u64>,
}

/// One evaluation's clock, ceilings and fault.
#[derive(Debug)]
pub struct Budget {
    started: Instant,
    deadline: Option<Instant>,
    cancel: CancelToken,
    memory_limit: Option<i64>,
    tuple_limit: Option<u64>,
    tuples: AtomicU64,
    memory: Arc<MemoryCounter>,
    stop: AtomicBool,
    fault: Mutex<Option<Diagnostic>>,
}

impl Budget {
    pub fn new(limits: &Limits) -> Arc<Self> {
        let started = Instant::now();
        Arc::new(Self {
            started,
            deadline: limits.time.map(|time| started + time),
            cancel: limits.cancel.clone().unwrap_or_default(),
            memory_limit: limits.memory_bytes.map(|bytes| bytes.min(i64::MAX as u64) as i64),
            tuple_limit: limits.tuples,
            tuples: AtomicU64::new(0),
            memory: Arc::new(MemoryCounter::default()),
            stop: AtomicBool::new(false),
            fault: Mutex::new(None),
        })
    }

    /// The counter the evaluation's threads charge.
    pub fn memory(&self) -> &Arc<MemoryCounter> {
        &self.memory
    }

    /// Whether operators should fall silent.
    #[inline]
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Record the first fault an operator met, and stop.
    pub fn fault(&self, diagnostic: Diagnostic) {
        let mut fault = self.fault.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if fault.is_none() {
            *fault = Some(diagnostic);
        }
        self.stop();
    }

    /// Charge materialized rows against the tuple ceiling.
    pub fn note_tuples(&self, count: u64) {
        let total = self.tuples.fetch_add(count, Ordering::Relaxed) + count;
        if let Some(limit) = self.tuple_limit {
            if total > limit {
                self.fault(Diagnostic::resource(format!(
                    "the evaluation materialized more than {limit} rows, its tuple ceiling"
                )));
            }
        }
    }

    pub fn tuples(&self) -> u64 {
        self.tuples.load(Ordering::Relaxed)
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// The reason the evaluation must stop now, if there is one. Checking
    /// records the reason, so a later `poll` returns the same answer.
    fn reason(&self) -> Option<Diagnostic> {
        if self.stopped() {
            let fault = self.fault.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(fault) = fault.as_ref() {
                return Some(fault.clone());
            }
        }
        let reason = if self.cancel.is_cancelled() {
            Some(Diagnostic::resource("the evaluation was cancelled"))
        } else if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            Some(Diagnostic::resource(format!(
                "the evaluation exceeded its time budget of {:.1}s",
                self.deadline
                    .map(|deadline| deadline.duration_since(self.started).as_secs_f64())
                    .unwrap_or_default()
            )))
        } else if let Some(limit) = self.memory_limit {
            let held = self.memory.current();
            (held > limit).then(|| {
                Diagnostic::resource(format!(
                    "the evaluation holds {held} bytes, more than its memory ceiling of {limit}"
                ))
            })
        } else {
            None
        };
        if let Some(reason) = reason {
            self.fault(reason.clone());
            return Some(reason);
        }
        None
    }

    /// Worker side: notice a reason to stop, without reporting it.
    pub fn observe(&self) {
        if !self.stopped() {
            let _ = self.reason();
        }
    }

    /// Coordinator side: the reason the evaluation stopped, as its result.
    pub fn poll(&self) -> Result<()> {
        match self.reason() {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    /// The fault recorded so far, if any.
    pub fn recorded(&self) -> Option<Diagnostic> {
        self.fault
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fault_stops_and_is_the_reason() {
        let budget = Budget::new(&Limits::default());
        assert!(budget.poll().is_ok());
        budget.fault(Diagnostic::evaluation("division by zero"));
        assert!(budget.stopped());
        let error = budget.poll().unwrap_err();
        assert_eq!(error.message, "division by zero");
    }

    #[test]
    fn cancellation_and_tuple_ceilings_are_reasons() {
        let cancel = CancelToken::new();
        let budget = Budget::new(&Limits {
            cancel: Some(cancel.clone()),
            tuples: Some(10),
            ..Limits::default()
        });
        budget.note_tuples(5);
        assert!(budget.poll().is_ok());
        budget.note_tuples(6);
        assert!(budget.poll().unwrap_err().message.contains("tuple ceiling"));

        let budget = Budget::new(&Limits {
            cancel: Some(cancel.clone()),
            ..Limits::default()
        });
        cancel.cancel();
        assert!(budget.poll().unwrap_err().message.contains("cancelled"));
    }

    #[test]
    fn a_tagged_thread_charges_its_counter() {
        let counter = Arc::new(MemoryCounter::default());
        {
            let _tag = MemoryTag::new(&counter);
            let held = vec![0u8; 1 << 20];
            std::hint::black_box(&held);
            drop(held);
        }
        // The allocation and the free were both charged, so the peak saw the
        // megabyte and the current figure returned towards zero.
        assert!(counter.peak() >= (1 << 20) as i64);
        assert!(counter.current() < (1 << 20) as i64);
    }
}

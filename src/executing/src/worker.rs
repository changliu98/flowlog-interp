//! The worker set of one evaluation.
//!
//! An evaluation owns its worker threads for its whole duration. It hands
//! them dataflows one after another - the strata of a cached run, or the
//! whole program at once - and each dataflow is built, fed and drained on
//! the same threads, so there is one thread start per evaluation rather than
//! one per stratum, and nothing an evaluation does can be confused with
//! another evaluation's work: its threads carry its memory tag, its budget
//! stops its operators, and a panic on one of its threads ends it alone.
//!
//! This is the deliberate alternative to a shared resident pool. A shared
//! pool could not cancel one evaluation without stopping the others, could
//! not attribute memory to the evaluation that allocated it, and could not
//! survive one evaluation's panic. Concurrency across evaluations is the
//! engine's admission limit; parallelism inside one is the worker count.

use crate::accounting::{Budget, MemoryTag};
use crate::dataflow::{Assembly, BuiltDataflow};
use parsing::diagnostic::{Diagnostic, Result};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::Thread;
use std::time::Duration;
use timely::communication::WorkerGuards;
use timely::worker::Worker;
use timely::Config;
use tracing::debug;

enum Job {
    Run(Arc<Run>),
    Shutdown,
}

struct Run {
    assembly: Arc<Assembly>,
    report: Sender<(usize, Result<()>)>,
}

impl Run {
    fn report(&self, index: usize, outcome: Result<()>) {
        let _ = self.report.send((index, outcome));
    }
}

/// The threads of one evaluation.
pub struct WorkerSet {
    senders: Vec<Sender<Job>>,
    threads: Vec<Thread>,
    guards: Option<WorkerGuards<()>>,
    poisoned: Arc<AtomicBool>,
    peers: usize,
}

impl WorkerSet {
    /// Start `workers` threads for an evaluation running under `budget`.
    pub fn start(workers: usize, budget: Arc<Budget>) -> Result<Self> {
        let workers = workers.max(1);
        let mut senders = Vec::with_capacity(workers);
        let mut receivers = Vec::with_capacity(workers);
        for _ in 0..workers {
            let (sender, receiver) = mpsc::channel::<Job>();
            senders.push(sender);
            receivers.push(Some(receiver));
        }
        let receivers = Arc::new(Mutex::new(receivers));
        let poisoned = Arc::new(AtomicBool::new(false));

        let closure_receivers = Arc::clone(&receivers);
        let closure_poisoned = Arc::clone(&poisoned);
        let closure_budget = Arc::clone(&budget);
        let guards = timely::execute(Config::process(workers), move |worker| {
            worker_loop(
                worker,
                &closure_receivers,
                &closure_poisoned,
                &closure_budget,
            );
        })
        .map_err(|error| {
            Diagnostic::internal(format!("cannot start the evaluation's worker threads: {error}"))
        })?;
        let threads = guards
            .guards()
            .iter()
            .map(|guard| guard.thread().clone())
            .collect();
        Ok(Self {
            senders,
            threads,
            guards: Some(guards),
            poisoned,
            peers: workers,
        })
    }

    pub fn peers(&self) -> usize {
        self.peers
    }

    /// Build `assembly` on every worker, feed it, and wait until it has
    /// drained on all of them, or until the evaluation's budget ends it.
    pub fn run(&self, assembly: Arc<Assembly>) -> Result<()> {
        if self.poisoned.load(Ordering::Relaxed) {
            return Err(Diagnostic::internal(
                "the evaluation's worker threads stopped after an earlier fault",
            ));
        }
        let budget = Arc::clone(&assembly.budget);
        let (report, outcomes) = mpsc::channel();
        let run = Arc::new(Run { assembly, report });
        for sender in &self.senders {
            sender.send(Job::Run(Arc::clone(&run))).map_err(|_| {
                Diagnostic::internal("an evaluation worker thread is gone")
            })?;
        }
        for thread in &self.threads {
            thread.unpark();
        }

        let mut reported = 0;
        let mut first_error: Option<Diagnostic> = None;
        while reported < self.peers {
            match outcomes.recv_timeout(Duration::from_millis(20)) {
                Ok((_, Ok(()))) => reported += 1,
                Ok((_, Err(diagnostic))) => {
                    reported += 1;
                    if first_error.is_none() {
                        first_error = Some(diagnostic);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.poisoned.load(Ordering::Relaxed) {
                        break;
                    }
                    // A reason to stop makes every operator fall silent; the
                    // dataflow then drains and the workers report.
                    budget.observe();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        if let Some(diagnostic) = first_error {
            return Err(diagnostic);
        }
        if self.poisoned.load(Ordering::Relaxed) {
            return Err(budget.recorded().unwrap_or_else(|| {
                Diagnostic::internal("an evaluation worker thread panicked")
            }));
        }
        budget.poll()
    }
}

impl Drop for WorkerSet {
    fn drop(&mut self) {
        for sender in &self.senders {
            let _ = sender.send(Job::Shutdown);
        }
        for thread in &self.threads {
            thread.unpark();
        }
        if let Some(guards) = self.guards.take() {
            for outcome in guards.join() {
                if let Err(error) = outcome {
                    debug!("evaluation worker thread ended with: {error}");
                }
            }
        }
    }
}

fn worker_loop(
    worker: &mut Worker,
    receivers: &Mutex<Vec<Option<Receiver<Job>>>>,
    poisoned: &AtomicBool,
    budget: &Arc<Budget>,
) {
    let index = worker.index();
    let receiver = receivers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_mut(index)
        .and_then(Option::take)
        .expect("every worker has a job receiver");
    let _tag = MemoryTag::new(budget.memory());
    let mut live: Vec<(Arc<Run>, BuiltDataflow)> = Vec::new();

    loop {
        if poisoned.load(Ordering::Relaxed) {
            return;
        }
        // Take every pending job: block for one when there is nothing to
        // step, otherwise only what is already there.
        let mut wait = live.is_empty();
        loop {
            let job = if wait {
                match receiver.recv() {
                    Ok(job) => job,
                    Err(_) => return,
                }
            } else {
                match receiver.try_recv() {
                    Ok(job) => job,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            };
            wait = false;
            match job {
                Job::Shutdown => return,
                Job::Run(run) => {
                    let built = catch_unwind(AssertUnwindSafe(|| run.assembly.build(worker)));
                    match built {
                        Ok(Ok(built)) => live.push((run, built)),
                        Ok(Err(diagnostic)) => run.report(index, Err(diagnostic)),
                        Err(panic) => {
                            poison(poisoned, budget, &live, Some(&run), panic, index);
                            return;
                        }
                    }
                }
            }
        }

        if live.is_empty() {
            continue;
        }
        let stepped = catch_unwind(AssertUnwindSafe(|| {
            worker.step_or_park(Some(Duration::from_millis(1)));
        }));
        if let Err(panic) = stepped {
            poison(poisoned, budget, &live, None, panic, index);
            return;
        }
        budget.observe();
        live.retain(|(run, built)| {
            if built.probe.done() {
                for capture in &built.captures {
                    capture.flush();
                }
                run.report(index, Ok(()));
                false
            } else {
                true
            }
        });
    }
}

/// A panic on a worker thread is an engine defect: record it as the
/// evaluation's fault, tell every job in flight, and make the other workers
/// leave.
fn poison(
    poisoned: &AtomicBool,
    budget: &Arc<Budget>,
    live: &[(Arc<Run>, BuiltDataflow)],
    building: Option<&Arc<Run>>,
    panic: Box<dyn std::any::Any + Send>,
    index: usize,
) {
    let message = if let Some(text) = panic.downcast_ref::<String>() {
        text.clone()
    } else if let Some(text) = panic.downcast_ref::<&str>() {
        (*text).to_string()
    } else {
        "a worker thread panicked without a text message".to_string()
    };
    let diagnostic = Diagnostic::internal(format!(
        "the engine failed inside its own dataflow on worker {index}: {message}"
    ))
    .with_detail(message);
    budget.fault(diagnostic.clone());
    poisoned.store(true, Ordering::Relaxed);
    for (run, _) in live {
        run.report(index, Err(diagnostic.clone()));
    }
    if let Some(run) = building {
        run.report(index, Err(diagnostic));
    }
}

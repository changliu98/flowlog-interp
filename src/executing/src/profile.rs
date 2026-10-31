//! Optional live operator accounting. Snapshots survive an external timeout.
//! Shared operators are charged once, to the rule that constructed them.
//!
//! Each worker logs into its own record, so the workers never wait for each
//! other here, and only for the writer while it reads their records for a
//! snapshot. Operators and channels are indexed by their worker-local
//! identifiers, which are small and dense.
use parsing::rule::FLRule;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use timely::logging::{StartStop, TimelyEvent, TimelyEventBuilder};
use timely::worker::Worker;

#[derive(Default)]
struct Operator {
    name: String,
    seconds: f64,
    activations: u64,
}

struct Active {
    id: usize,
    started: Duration,
    children: Duration,
}

#[derive(Default)]
struct RuleCost {
    head: String,
    recursive: bool,
    build_seconds: f64,
}

#[derive(Default)]
struct State {
    phase: String,
    /// The embedded blocks: compiling, ready, or failed; none without blocks.
    helpers: Option<String>,
    complete: bool,
    rules: BTreeMap<usize, RuleCost>,
}

/// What one worker's operators did, by operator and channel identifier.
#[derive(Default)]
struct WorkerLog {
    operators: Vec<Operator>,
    /// The rule line that constructed each operator.
    owners: Vec<Option<usize>>,
    addresses: HashMap<Vec<usize>, usize>,
    /// The address of each channel's source operator.
    channels: Vec<Option<Vec<usize>>>,
    /// Records sent on each channel.
    records: Vec<i64>,
    active: Vec<Active>,
}

fn slot<T: Default>(items: &mut Vec<T>, index: usize) -> &mut T {
    if index >= items.len() {
        items.resize_with(index + 1, T::default);
    }
    &mut items[index]
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct Profile {
    path: PathBuf,
    started: Instant,
    state: Mutex<State>,
    /// One log per worker, by worker index.
    workers: Mutex<Vec<Arc<Mutex<WorkerLog>>>>,
}

pub struct ProfileRun {
    pub profile: Arc<Profile>,
    stop: mpsc::Sender<()>,
    writer: Option<thread::JoinHandle<()>>,
}

impl ProfileRun {
    pub fn start(path: Option<&PathBuf>) -> Option<Self> {
        let profile = Arc::new(Profile {
            path: path?.clone(),
            started: Instant::now(),
            state: Mutex::new(State { phase: "planning".into(), ..State::default() }),
            workers: Mutex::new(Vec::new()),
        });
        profile.write();
        let (stop, receive) = mpsc::channel();
        let background = Arc::clone(&profile);
        let writer = thread::spawn(move || {
            loop {
                match receive.recv_timeout(Duration::from_millis(500)) {
                    Err(mpsc::RecvTimeoutError::Timeout) => background.write(),
                    _ => break,
                }
            }
            background.write();
        });
        Some(Self { profile, stop, writer: Some(writer) })
    }
}

impl Drop for ProfileRun {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

impl Profile {
    pub fn phase(&self, phase: &str) {
        lock(&self.state).phase = phase.into();
    }

    /// Where building the embedded blocks stands; it overlaps execution.
    pub fn helpers(&self, status: &str) {
        lock(&self.state).helpers = Some(status.into());
    }

    pub fn complete(&self) {
        let mut state = lock(&self.state);
        state.complete = true;
        state.phase = "completed".into();
    }

    /// The log of worker `index`, made on first use.
    fn worker_log(&self, index: usize) -> Arc<Mutex<WorkerLog>> {
        Arc::clone(slot(&mut lock(&self.workers), index))
    }

    pub fn attach(self: &Arc<Self>, worker: &Worker) {
        let log = self.worker_log(worker.index());
        if let Some(mut registry) = worker.log_register() {
            registry.insert::<TimelyEventBuilder, _>("timely", move |_now, batch| {
                if let Some(events) = batch.take() {
                    let mut log = lock(&log);
                    let log = &mut *log;
                    for (at, event) in events {
                        match event {
                            TimelyEvent::Operates(event) => {
                                slot(&mut log.operators, event.id).name = event.name;
                                log.addresses.insert(event.addr, event.id);
                            }
                            TimelyEvent::Channels(event) => {
                                let mut address = event.scope_addr;
                                address.push(event.source.0);
                                *slot(&mut log.channels, event.id) = Some(address);
                            }
                            TimelyEvent::Messages(event) if event.is_send => {
                                *slot(&mut log.records, event.channel) += event.record_count;
                            }
                            TimelyEvent::Schedule(event) => match event.start_stop {
                                StartStop::Start => {
                                    log.active.push(Active {
                                        id: event.id, started: at, children: Duration::ZERO,
                                    });
                                }
                                StartStop::Stop => {
                                    if let Some(active) = log.active.pop() {
                                        if active.id == event.id {
                                            let elapsed = at.saturating_sub(active.started);
                                            let exclusive = elapsed.saturating_sub(active.children);
                                            if let Some(parent) = log.active.last_mut() {
                                                parent.children += elapsed;
                                            }
                                            let operator = slot(&mut log.operators, event.id);
                                            operator.seconds += exclusive.as_secs_f64();
                                            operator.activations += 1;
                                        } else {
                                            log.active.clear();
                                        }
                                    }
                                }
                            },
                            _ => {}
                        }
                    }
                }
            });
        }
    }

    pub fn rule<'a>(self: &Arc<Self>, worker: &'a Worker, rule: &FLRule, recursive: bool) -> RuleScope<'a> {
        RuleScope {
            profile: Arc::clone(self), worker, first: worker.peek_identifier(),
            line: rule.line(), head: rule.head().name().to_string(),
            recursive, started: Instant::now(),
        }
    }

    fn snapshot(&self) -> Value {
        let state = lock(&self.state);
        let logs: Vec<Arc<Mutex<WorkerLog>>> = lock(&self.workers).clone();
        // Per rule line: exclusive seconds, activations, records sent.
        let mut totals: HashMap<usize, (f64, u64, i64)> = HashMap::new();
        let mut unattributed = 0.0;
        let mut operators = Vec::new();
        let mut active = Vec::new();
        for (worker, log) in logs.iter().enumerate() {
            let log = lock(log);
            let owner = |id: usize| log.owners.get(id).copied().flatten();
            // This worker's eight busiest operators: (seconds, operator).
            let mut busiest: Vec<(f64, usize)> = Vec::new();
            for (id, operator) in log.operators.iter().enumerate() {
                match owner(id) {
                    Some(line) => {
                        let total = totals.entry(line).or_default();
                        total.0 += operator.seconds;
                        total.1 += operator.activations;
                    }
                    None => unattributed += operator.seconds,
                }
                if operator.seconds > 0.0 {
                    busiest.push((operator.seconds, id));
                }
            }
            let by_seconds = |a: &(f64, usize), b: &(f64, usize)| b.0.total_cmp(&a.0);
            if busiest.len() > 8 {
                busiest.select_nth_unstable_by(7, by_seconds);
                busiest.truncate(8);
            }
            for &(seconds, id) in &busiest {
                let operator = &log.operators[id];
                operators.push(json!({"worker": worker, "operator": id,
                    "name": operator.name, "rule_line": owner(id),
                    "seconds": seconds, "activations": operator.activations}));
            }
            for (channel, &records) in log.records.iter().enumerate() {
                if records == 0 {
                    continue;
                }
                let line = log.channels.get(channel).and_then(Option::as_ref)
                    .and_then(|address| log.addresses.get(address.as_slice()))
                    .and_then(|&id| owner(id));
                if let Some(line) = line {
                    totals.entry(line).or_default().2 += records;
                }
            }
            if let Some(item) = log.active.last() {
                active.push(json!({"worker":worker,"operator":item.id,
                    "rule_line":owner(item.id),
                    "note":"Last observed open schedule event; buffered events may lag."}));
            }
        }
        operators.sort_by(|a, b| b["seconds"].as_f64().unwrap_or_default().total_cmp(&a["seconds"].as_f64().unwrap_or_default()));
        operators.truncate(8);

        // Rules in line order, then by seconds; the most records are ordered
        // after that by records, the earlier order breaking ties.
        let mut rules: Vec<(usize, &RuleCost, (f64, u64, i64))> = state
            .rules
            .iter()
            .map(|(&line, rule)| (line, rule, totals.get(&line).copied().unwrap_or_default()))
            .collect();
        let row = |(line, rule, total): &(usize, &RuleCost, (f64, u64, i64))| {
            json!({"line": line, "head": rule.head, "recursive": rule.recursive,
                "seconds": total.0, "build_seconds": rule.build_seconds,
                "activations": total.1, "operator_records": total.2})
        };
        rules.sort_by(|a, b| b.2 .0.total_cmp(&a.2 .0));
        let hottest: Vec<Value> = rules.iter().take(16).map(row).collect();
        rules.sort_by_key(|rule| std::cmp::Reverse(rule.2 .2));
        let most_records: Vec<Value> = rules.iter().take(16).map(row).collect();
        json!({"version":1,"elapsed_seconds":self.started.elapsed().as_secs_f64(),
            "phase":state.phase,"helpers":state.helpers,"evaluation_complete":state.complete,
            "hottest":hottest,"most_records":most_records,
            "operators":operators,
            "last_observed_active":active,"unattributed_operator_seconds":unattributed,
            "note":"Exclusive time from completed operator schedule events, summed across workers. Shared operators are charged once to their constructing rule, not exclusively caused by it. Record traffic counts intermediate messages, including duplicates/retractions, not distinct facts. Buffered and unfinished operator work may be absent; this is partial diagnostic evidence."})
    }

    fn write(&self) {
        let value = self.snapshot();
        let temporary = self.path.with_extension("json.tmp");
        if let Ok(bytes) = serde_json::to_vec(&value) {
            if std::fs::write(&temporary, bytes).is_ok() {
                let _ = std::fs::rename(temporary, &self.path);
            }
        }
    }
}

pub struct RuleScope<'a> {
    profile: Arc<Profile>,
    worker: &'a Worker,
    first: usize,
    line: usize,
    head: String,
    recursive: bool,
    started: Instant,
}

impl Drop for RuleScope<'_> {
    fn drop(&mut self) {
        {
            let mut state = lock(&self.profile.state);
            let rule = state.rules.entry(self.line).or_default();
            rule.head = self.head.clone();
            rule.recursive |= self.recursive;
            rule.build_seconds += self.started.elapsed().as_secs_f64();
        }
        let log = self.profile.worker_log(self.worker.index());
        let mut log = lock(&log);
        for id in self.first..self.worker.peek_identifier() {
            slot(&mut log.owners, id).get_or_insert(self.line);
        }
    }
}

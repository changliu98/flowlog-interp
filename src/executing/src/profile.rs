//! Optional live operator accounting. Snapshots survive an external timeout.
//! Shared operators are charged once, to the rule that constructed them.
use parsing::rule::FLRule;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
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
    complete: bool,
    rules: BTreeMap<usize, RuleCost>,
    owners: HashMap<(usize, usize), usize>,
    operators: HashMap<(usize, usize), Operator>,
    addresses: HashMap<(usize, Vec<usize>), usize>,
    channels: HashMap<(usize, usize), Vec<usize>>,
    records: HashMap<(usize, usize), i64>,
    active: HashMap<usize, Vec<Active>>,
}

pub struct Profile {
    path: PathBuf,
    started: Instant,
    state: Mutex<State>,
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
        self.state.lock().unwrap_or_else(|p| p.into_inner()).phase = phase.into();
    }

    pub fn complete(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.complete = true;
        state.phase = "completed".into();
    }

    pub fn attach(self: &Arc<Self>, worker: &Worker) {
        let index = worker.index();
        let profile = Arc::clone(self);
        if let Some(mut registry) = worker.log_register() {
            registry.insert::<TimelyEventBuilder, _>("timely", move |_now, batch| {
                if let Some(events) = batch.take() {
                    let mut state = profile.state.lock().unwrap_or_else(|p| p.into_inner());
                    for (at, event) in events {
                        match event {
                            TimelyEvent::Operates(event) => {
                                state.addresses.insert((index, event.addr), event.id);
                                state.operators.entry((index, event.id)).or_default().name = event.name;
                            }
                            TimelyEvent::Channels(event) => {
                                let mut address = event.scope_addr;
                                address.push(event.source.0);
                                state.channels.insert((index, event.id), address);
                            }
                            TimelyEvent::Messages(event) if event.is_send => {
                                *state.records.entry((index, event.channel)).or_default() += event.record_count;
                            }
                            TimelyEvent::Schedule(event) => match event.start_stop {
                                StartStop::Start => {
                                    state.active.entry(index).or_default().push(Active {
                                        id: event.id, started: at, children: Duration::ZERO,
                                    });
                                }
                                StartStop::Stop => {
                                    let stack = state.active.entry(index).or_default();
                                    if let Some(active) = stack.pop() {
                                        if active.id == event.id {
                                            let elapsed = at.saturating_sub(active.started);
                                            let exclusive = elapsed.saturating_sub(active.children);
                                            if let Some(parent) = stack.last_mut() {
                                                parent.children += elapsed;
                                            }
                                            let operator = state.operators.entry((index, event.id)).or_default();
                                            operator.seconds += exclusive.as_secs_f64();
                                            operator.activations += 1;
                                        } else {
                                            stack.clear();
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
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let mut totals: BTreeMap<usize, (f64, u64, i64)> = BTreeMap::new();
        let mut unattributed = 0.0;
        let mut operators = Vec::new();
        for (key, operator) in &state.operators {
            let line = state.owners.get(key).copied();
            if let Some(line) = line {
                let total = totals.entry(line).or_default();
                total.0 += operator.seconds;
                total.1 += operator.activations;
            } else {
                unattributed += operator.seconds;
            }
            if operator.seconds > 0.0 {
                operators.push(json!({"worker": key.0, "operator": key.1,
                    "name": operator.name, "rule_line": line,
                    "seconds": operator.seconds, "activations": operator.activations}));
            }
        }
        for (key, records) in &state.records {
            if let Some(address) = state.channels.get(key) {
                if let Some(id) = state.addresses.get(&(key.0, address.clone())) {
                    if let Some(line) = state.owners.get(&(key.0, *id)) {
                        totals.entry(*line).or_default().2 += records;
                    }
                }
            }
        }
        let mut rules: Vec<Value> = state.rules.iter().map(|(line, rule)| {
            let total = totals.get(line).copied().unwrap_or_default();
            json!({"line": line, "head": rule.head, "recursive": rule.recursive,
                "seconds": total.0, "build_seconds": rule.build_seconds,
                "activations": total.1, "operator_records": total.2})
        }).collect();
        rules.sort_by(|a,b| b["seconds"].as_f64().unwrap_or_default().total_cmp(&a["seconds"].as_f64().unwrap_or_default()));
        let hottest: Vec<_> = rules.iter().take(16).cloned().collect();
        rules.sort_by_key(|row| std::cmp::Reverse(row["operator_records"].as_i64().unwrap_or_default()));
        operators.sort_by(|a,b| b["seconds"].as_f64().unwrap_or_default().total_cmp(&a["seconds"].as_f64().unwrap_or_default()));
        let active: Vec<_> = state.active.iter().filter_map(|(worker, stack)| {
            stack.last().map(|item| json!({"worker":worker,"operator":item.id,
                "rule_line":state.owners.get(&(*worker,item.id)),
                "note":"Last observed open schedule event; buffered events may lag."}))
        }).collect();
        json!({"version":1,"elapsed_seconds":self.started.elapsed().as_secs_f64(),
            "phase":state.phase,"evaluation_complete":state.complete,
            "hottest":hottest,"most_records":rules.into_iter().take(16).collect::<Vec<_>>(),
            "operators":operators.into_iter().take(8).collect::<Vec<_>>(),
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
        let mut state = self.profile.state.lock().unwrap_or_else(|p| p.into_inner());
        let rule = state.rules.entry(self.line).or_default();
        rule.head = self.head.clone();
        rule.recursive |= self.recursive;
        rule.build_seconds += self.started.elapsed().as_secs_f64();
        for id in self.first..self.worker.peek_identifier() {
            state.owners.entry((self.worker.index(), id)).or_insert(self.line);
        }
    }
}

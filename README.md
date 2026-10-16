<p align="center">
  <img src="flowlog_full.png" alt="FlowLog Logo" width="400"/>
</p>

<p align="center">
  <strong>An efficient, scalable, and extensible Datalog engine built atop Differential Dataflow</strong>
</p>

<p align="center">
  <a href="https://arxiv.org/pdf/2511.00865">Paper</a> •
  <a href="#quick-example">Quick Start</a> •
  <a href="#the-engine-as-a-library">Library</a> •
  <a href="#the-dialect">Dialect</a> •
  <a href="#evaluation">Evaluation</a> •
  <a href="#diagnostics">Diagnostics</a>
</p>

---

## Archive Notice

This repository is a public archive of the paper's engine. Active development
and maintenance have moved to [this repo](https://github.com/flowlog-rs/FlowLog).
The fork here extends the archive into a general-purpose engine with a
published contract: a library interface, a typed value domain, embedded
functions, structured diagnostics, per-evaluation limits, a content-addressed
state cache and one-step provenance. Everything below the "Paper" section
describes that contract.

---

## FlowLog Paper

This repo contains the implementation for the paper:

**FlowLog: Efficient and Extensible Datalog via Incrementality**  
Hangdong Zhao, Zhenghong Yu, Srinag Rao, Simon Frisk, Zhiwei Fan, Paraschos Koutris  
VLDB 2026 (Boston)

[**Read the paper on arXiv**](https://arxiv.org/pdf/2511.00865)

---

## Architecture

```
├── parsing       # grammar, AST, validation and typing, diagnostics
├── strata        # stratification
├── planning      # logical plans per rule, row programs, group plans
  ├── catalog       # per-rule metadata
  └── optimizing    # join order
└── executing     # the engine: dataflows, worker sets, cache, symbols, API
  ├── reading       # rows, relations, arrangements, file reading
  └── macros        # code generation for each differential operator
```

The `executing` package builds the library `flowlog` (an rlib for Rust hosts
and `libflowlog.so` for the C interface) and two binaries: `executing`, the
command line, and `flowlogctl`, a client of the service. The command line, the
service and the C interface are clients of one type, `flowlog::Engine`, and
add nothing it does not have.

---

## Quick Example

### Environment Setup

```bash
bash tool/env.sh        # pins the toolchain the paper used
rustc --version
```

Embedded functions need `rustc` at run time (see below); nothing else does.

### Write a Simple Program

```datalog
.in
.decl Source(id: number)
.input Source.csv

.decl Arc(x: number, y: number)
.input Arc.csv

.printsize
.decl Reach(id: number)

.rule
Reach(y) :- Source(y).
Reach(y) :- Reach(x), Arc(x, y).
```

### Build and Run

```bash
cargo build --release
target/release/executing -p reach.dl -f reach -c output -w 8
```

The release build trades build time for speed: the whole program is
optimized as one unit (fat LTO, one codegen unit) for the CPU of the machine
that builds it (`target-cpu=native` in `.cargo/config.toml`). It takes about
an hour, and the binary runs only on CPUs with the builder's instruction set.
A quick build for development:
`CARGO_PROFILE_RELEASE_LTO=off CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 cargo build --release`.

Every log line goes to standard error; standard output carries data only
(the `--check` report).

---

## The engine as a library

### Rust

```rust
use flowlog::{Engine, EngineConfig, EvaluationRequest, EvaluationOptions, Inputs, ProgramSource};
use std::collections::BTreeMap;

let engine = Engine::new(EngineConfig { workers: 4, ..EngineConfig::default() });

let program = ".in\n.decl E(k: number, s: symbol)\n.printsize\n.decl R(s: symbol)\n\
               .rule\nR(s) :- E(k, s), k > 1.\n";
let report = engine.check(program, "example.dl")?;          // parse, validate, stratify, plan

let main = engine.intern("main")?;                            // a symbol's cell
let mut rows = BTreeMap::new();
rows.insert("E".to_string(), vec![vec![1, main], vec![2, main]]);
let result = engine.evaluate(EvaluationRequest {
    program: ProgramSource::Text { name: "example.dl".into(), source: program.into() },
    inputs: Inputs::Rows(rows),
    options: EvaluationOptions::default(),
})?;
let r = &result.outputs["R"];                                 // every relation's final state
assert_eq!(engine.symbol_text(r.rows[0][0]).as_deref(), Some("main"));
println!("{:?}", result.stats);                               // the run's counters
```

`Engine::evaluate` never panics: a defect in the engine's own code comes back
as a diagnostic of kind `internal`. `EvaluationOptions` carries the limits,
whether to read along the cache, the schedule, and the rows to explain.
The state cache is a directory, and nothing else: an engine caches only when
it has one (`EngineConfig::cache_dir`, the CLI's `--cache-dir`), and then
every evaluation reads and writes it unless its options opt out
(`cache: Some(false)`, JSON `"cache": false`). Without a directory nothing is
cached, whatever a request asks, and an evaluation is one whole-program
dataflow.

### C

`libflowlog.so` exports the interface documented in `src/executing/src/capi.rs`:

```c
flowlog_engine* flowlog_engine_new(const char* config_json, char** error_json);
char*           flowlog_check(flowlog_engine*, const char* program, const char* name);
int64_t         flowlog_intern(flowlog_engine*, const uint8_t* text, size_t length);
int             flowlog_symbol(flowlog_engine*, int64_t id, const uint8_t** text, size_t* length);
flowlog_result* flowlog_evaluate(flowlog_engine*, const char* request_json,
                                 const flowlog_input* inputs, size_t input_count);
int             flowlog_result_ok(const flowlog_result*);
char*           flowlog_result_json(const flowlog_result*);        /* stats, witnesses, diagnostic */
size_t          flowlog_result_relation_count(const flowlog_result*);
int             flowlog_result_relation(const flowlog_result*, size_t index, const char** name,
                                        size_t* arity, size_t* rows, const int64_t** cells,
                                        const char** types);
void            flowlog_result_free(flowlog_result*);
void            flowlog_engine_free(flowlog_engine*);
void            flowlog_string_free(char*);
```

Inputs and outputs are flat row-major `int64_t` cell arrays; symbols cross as
ids. `config_json` has the fields of `EngineConfig`; `request_json` is
`{"program": {"text", "name"}, "options": {...}}` with the same options as the
service. Every failure is a JSON diagnostic.

### The service

`executing --daemon-socket <path>` serves one engine on a Unix-domain socket,
protocol version 2: one JSON object per request line, one per response line.
Evaluations run concurrently, each on its own thread, under the engine's
admission limit (`--max-concurrent`).

```bash
target/release/executing -p reach.dl -f reach -c output -w 8 \
  --daemon-socket /tmp/flowlog.sock --cache-dir /tmp/flowlog-states

target/release/flowlogctl -s /tmp/flowlog.sock evaluate --id one --program other.dl \
  --facts other-facts --csvs other-output --inline --budget-seconds 30 --explain
target/release/flowlogctl -s /tmp/flowlog.sock check --program other.dl
target/release/flowlogctl -s /tmp/flowlog.sock cancel --id one
target/release/flowlogctl -s /tmp/flowlog.sock stats
target/release/flowlogctl -s /tmp/flowlog.sock shutdown
```

Commands: `evaluate` (`program` as `{"path"}` or `{"text","name"}`; `inputs`
as `{"facts": dir}` or `{"rows": {relation: [[cell, ...], ...]}}` where a
symbol cell is its text; `output` as `{"csvs": dir}` and/or `{"inline": true}`;
`options` with `budget_seconds`, `memory_limit_bytes`, `tuple_limit`, `cache`,
`schedule`, `explain`, `explain_all`), `check`, `cancel`, `stats`, `shutdown`,
and `reload`, the version 1 form whose paths default to the ones the service
was started with. Every response carries `version`, `ok`, `command`, the
request's `id`, the cache occupancy, and on failure a structured `diagnostic`
beside its rendering in `error`.

---

## The dialect

### Declarations

```datalog
.in
.decl Edge(x: number, y: number)      // an input relation, read from Edge.facts
.input Edge.csv                       // ...or from this file
.decl Named(name: symbol, k: number)  // `string` is accepted as a spelling of `symbol`
.printsize
.decl Reach(x: number, y: number)     // an output relation, written when -c is given
.decl Any()                           // arity 0: one row, or none
```

A relation no `.decl` names may still be derived and read; validation infers
its column types from the rules that derive it.

### Rules

```datalog
reach(y) :- reach(x), edge(x, y).                      // recursion
two_hops(x, z) :- edge(x, y), edge(y, z), x != z.       // comparisons
indirect(x, z) :- edge(x, y), edge(y, z), !edge(x, z).  // negation (stratified)
count_paths(x, z, count(y)) :- edge(x, y), edge(y, z).  // aggregation, in any column
best(min(cost), x) :- offer(x, cost).
total(x, sum(v * 10 + 1)) :- item(x, v).                // aggregation over an expression
labelled(x, 7, x + y) :- edge(x, y).                    // head constants and head arithmetic
main_edge(y) :- edge("main", y).                        // symbol literals
Any() :- edge(x, y), x > 5.
```

- A body needs at least one positive atom. Every variable of a negated atom,
  a comparison or the head must be bound by a positive atom or by an earlier
  call.
- Arithmetic is `+ - * / %`, evaluated left to right without precedence over
  numbers, wrapping on overflow; a zero divisor faults the evaluation. Ordered
  comparison is defined over numbers; equality over two values of one type.
- At most one aggregate per head, `count`, `sum`, `min` or `max`, in any
  column, over a variable or an expression; `sum`, `min` and `max` take
  numbers. Every rule deriving one relation aggregates the same column with
  the same operator, or none at all.
- `True` in a body is dropped; `False` makes the rule derive nothing.
- Rules may carry `.plan`, `.sip` or `.optimize` hints; `-O` overrides them.

### Values

A cell is a signed 64-bit integer, `Val`. A `number` column holds the value
itself. A `symbol` column holds text as an id: the first eight bytes of the
SHA-256 of the UTF-8 text, big-endian, sign bit cleared. The id is a function
of the text alone, so every engine agrees on it and a host can compute it
without a round trip (`flowlog::symbols::symbol_id`). The engine's symbol
table maps ids back to texts and refuses the one collision content-derived
ids can meet, two texts under one id.

### What validation refuses

A duplicate declaration; a rule with no positive atom; an unbound negated,
compared or head variable; more than one aggregate in a head; rules that
aggregate one relation differently; a head or body arity that disagrees with
a declaration or with another use; a variable typed two ways; a constant of
the wrong type in a column; arithmetic or ordering over a symbol; an aggregate
`sum`/`min`/`max` over a symbol; a call to an unknown function, with the wrong
arity, with an unbound or wildcard argument, or with an argument of the wrong
type; binding a `bool` function or filtering by a non-`bool` one; a negated
atom that retains no column (a test on the whole relation, not a join). Each
is a `validation` diagnostic naming the rule and its line.

---

## Embedded functions

A program keeps row-local imperative logic in `.code rust` ... `.endcode`
sections. Each section is a *block*: a compilation unit with its own `use`s,
helpers, types and constants, whose public top-level functions are callable
from rules. A program may hold any number of blocks; their function names
share one namespace, and a function cannot call into another block.

```datalog
.code rust
pub fn normalize(value: i64, limit: i64) -> i64 { value.saturating_abs().min(limit) }
pub fn acceptable(value: i64) -> bool { value % 2 == 0 }
pub fn suffixed(name: Symbol) -> Symbol { Symbol::new(&format!("{}_1", name.as_str())) }
.endcode

.rule
Output(X, Y) :- Input(X), Y = @call(normalize, X, 255), @call(acceptable, Y).
Renamed(R, K) :- Named(N, K), R = @call(suffixed, N).
```

- A parameter is `i64` for a `number` or `Symbol` for a `symbol`; a result is
  `i64`, `bool` or `Symbol`. An `i64`- or `Symbol`-returning call binds a
  variable with `Y = @call(f, ...)`; a `bool`-returning call is written bare
  and drops the row when it returns `false`. Arguments are constants,
  variables bound by positive atoms, and results of earlier calls; a
  comparison may read a call result.
- `Symbol` is a type the engine provides to every block: `Symbol::new(&str)`
  interns a text, `symbol.as_str()` reads one, `symbol.id()` is the cell.
- Functions are pure by contract: deterministic, no I/O, no clock, no
  randomness, no external state. Differential dataflow may run, repeat and
  reorder a call across worker threads.
- A panic inside a function ends the evaluation with a `function` diagnostic
  naming the function, the line of its block as written, the program line,
  and the rule that called it. It never unwinds into the engine.
- Each block is compiled by `rustc` into a shared library named by the digest
  of the complete generated source (including the engine's scaffolding and
  call wrappers), the compiler version and the call interface version
  (`CALL_ABI_VERSION`). By default, it compiles in a temporary build directory
  removed when the loaded module is released; it does not create a persistent
  cache in the home, XDG, or temporary directory. `--call-cache` or
  `FLOWLOG_CALL_CACHE` explicitly opts into persistent compiled libraries.
  Only that explicit store reuses libraries across evaluations. A library that
  cannot load or lacks the panic channel or any call wrapper is rebuilt once and
  replaced atomically. If repair fails, a `function` diagnostic names the
  cache file to remove and reports both failures. A block that does not
  compile carries rustc's report with lines counted inside the block.
- The call interface is documented in `src/executing/src/native_calls.rs`:
  one `i64` cell per argument and result, and a context of two callbacks over
  the engine's symbol table. Embedded Rust is trusted native code and runs
  with the engine's privileges.

Sideways information passing applies to rules with calls; the row program
stays attached to the final rule of the rewrite.

---

## Evaluation

### Row programs

A rule's joins produce one row per body solution. Everything row-local that
remains - calls, comparisons over call results, head arithmetic, head
constants, the expression under an aggregate - is one *row program*, a
straight-line sequence of steps over that row followed by the head
projection. A rule that needs none of it has no row program.

### Schedules

- **Whole program** (the default without a cache directory): every
  stratum in one dataflow, sharing intermediates across strata. Never cached.
- **Per stratum** (the default with a cache directory, or when asked for):
  one dataflow per stratum, each unit keyed against the state cache.

Both run on the evaluation's own worker set: `workers` threads started for the
evaluation, handed every dataflow of it in turn, and joined when it ends. The
set is not shared between evaluations, which is what lets one evaluation be
cancelled, charged for its memory, and fail on its own. Concurrency across
evaluations is the engine's admission limit (`max_concurrent`, 0 for none).

The runtime uses Differential Dataflow 0.25 and Timely 0.31. Directory inputs
use the configured worker count to read and parse disjoint byte ranges,
aligned to complete lines. Inputs and captures use compact rows while sorting;
large boundaries merge in parallel before producing the public row vectors.
Each worker publishes its capture after the frontier completes, preserving
signed retractions across workers. These paths serve both schedules, including
service reloads. A worker its dataflow gives nothing to do spins for 50 us,
then parks until a peer's message or a new job wakes it, so waiting workers
leave their cores to the working ones and to other evaluations; idle workers
block on their job receiver.

For a reproducible release-build comparison, build the `runtime_bench`
example in each checkout and retain each binary under a separate name. Use a
separate Cargo target directory for each checkout:

```sh
CARGO_TARGET_DIR=target/perf-after cargo build --release --locked -p executing --example runtime_bench
python3 scripts/benchmark_runtime.py --output target/runtime-comparison \
  --engine before /path/to/before-runtime_bench \
  --engine after target/perf-after/release/examples/runtime_bench
```

The benchmark uses a million-row scan and disconnected-chain reachability,
with three repetitions at 1, 4, and 8 workers. It measures whole-program,
cold-cache, and resident-cache evaluations separately and verifies every
result's row count and digest against independently computed expected rows.
Its manifest records input and binary hashes, run order, and timing scope;
the output directory also retains raw measurements and median timings. See
[the recorded comparison](docs/runtime-performance-2026-09-10.md).

For the official FlowLog Bench cohort at 32 workers, see
[the performance audit and measurements](docs/performance-w32-2026-09-10.md)
and `scripts/benchmark_flowlog_bench.sh`.

### Limits

An evaluation runs under `Limits`: a wall-clock `time`, a `cancel` token the
caller may trip from another thread, `memory_bytes` its threads may hold at
once (the allocator attributes every allocation and free on the evaluation's
threads to it; an estimate exact when an evaluation frees what it allocated),
and `tuples` it may materialize at unit boundaries. When any is crossed, or
an operator faults, every operator of the evaluation falls silent, the
dataflow drains, and the evaluation returns a `resource` (or the fault's)
diagnostic. The check sits between operators: a function that never returns
cannot be stopped.

### The state cache

The unit is one recursive stratum, or one relation's rules within a
non-recursive stratum. A unit's key is content on both sides:

- the canonical text of its rules - variables numbered by first appearance,
  body predicates in a name-free order, planning hints dropped, so a renamed,
  reordered or duplicated rule spells the same;
- the column types of every relation it touches;
- the source of every block a rule of the unit calls into, and no other block;
- the digest of every input relation's rows.

Evaluation reads along the strata: every key of a stratum is computed from
settled states, the units the cache holds are served, and the rest are
assembled into one dataflow over the injected input states, captured at their
heads, and stored. An upstream edit that leaves a relation's rows unchanged
stops invalidating there; an unrelated declaration, clause or block touches
nothing.

An exact unit miss for a non-recursive, non-aggregate head first looks up each
distinct canonical rule's *contribution* in the same store, keyed by its body
inputs alone. The head is the set union of its active contributions and any
rows inherited from earlier strata: adding or replacing a clause reuses the
unchanged clauses; deleting one preserves tuples still supported by another.
A union whose contributions all hit needs no dataflow.

The cache is a directory (`cache_dir`, `--cache-dir`), bounded by
`cache_disk_bytes` (32 GiB by default) and shared by every process that points
at it; a process keeps what it reads and writes there in memory too, bounded
by `cache_memory_bytes` and evicted least recently used. The directory is laid
out for a slow, seek-bound disk:

```text
<cache_dir>/FORMAT              the layout: "flowlog-state-store 4"
<cache_dir>/index.log           append-only records, every process's
<cache_dir>/packs/<id>.pack     what one process wrote to one shelf, in the order it computed it
<cache_dir>/.maintenance.lock   held by the one process evicting or compacting
<cache_dir>/.index.lock         shared while appending, exclusive while the index is replaced
```

- A relation's rows are a blob named by their digest and stored once, however
  many entries hold them (two processes computing the same rows at once may
  each store them); an entry names the digests its unit settled.
- A process appends everything it stores to packs of its own, so writing is
  sequential and needs no per-entry file, rename or sync. Unit states and rule
  contributions go to separate packs: an evaluation whose units all hit reads
  unit states only, stratum after stratum, as they were written.
- The index is read whole when a process first needs it and tailed between
  strata, so a lookup touches no file; the hits of a stratum are read sorted
  by pack and offset, nearby blobs merged into one read, and the kernel is
  asked for the reads ahead of the readers, so the disk sees them together
  and orders them.
- Over budget, the process that notices removes whole packs, least recently
  used first, under a nonblocking lock, and rewrites the index without them.
  A pack read or written in the last ten minutes, or still held by the live
  process writing it, is never removed; appenders wait the moment the new
  index takes the old one's place, so no record is lost to it.
- Index records carry a check and blobs their digest; nothing is synced, and
  whatever a crash left short reads as absent.

An entry carries the texts of the symbols in its rows, so a process that
never interned them reads them by name. The cache is not a source of truth:
a miss costs one evaluation. Version 4 neither reads nor removes the
one-file-per-entry layout of earlier versions: delete its `<cache_dir>/??/`
directories and `<cache_dir>/.sweep.lock`.

What the cache is not: it retains no dataflow topology and does not
incrementally update a recursive fixed point. A missed recursive or aggregate
unit is recomputed whole.

### Counters

Every evaluation returns `CacheRunStats`, and a cached command-line run writes
it to `<csvs>/csvs/cache-stats.json`:

| counter | meaning |
|---|---|
| `strata`, `units` | strata of the program; units keyed |
| `hits`, `misses`, `disk_hits` | units served / evaluated; hits served from disk |
| `cutoff_hits` | hits keyed after an earlier miss of the run (an upper bound on early cutoff, independent units included) |
| `contribution_hits`, `contribution_misses`, `contribution_disk_hits` | rule-level lookups inside missed ordinary units |
| `rules_evaluated` | source rules assembled into dataflows |
| `rows_loaded`, `rows_cached`, `contribution_rows_*` | rows served / stored, whole units and contributions separately |
| `planning_micros`, `execution_micros`, `cache_micros`, `output_micros`, `total_micros` | preparation, dataflow, cache bookkeeping, output, the whole run |
| `disk_sweep_*` | the cache directory's maintenance: passes, packs examined and removed, bytes removed |
| `disk_reads`, `disk_read_bytes` | reads issued to the cache directory after merging nearby blobs, and the bytes they covered |
| `disk_written_bytes`, `disk_shared_states` | bytes this run appended; states it found stored already and did not write again |
| `entries`, `resident_rows`, `resident_bytes`, `max_bytes` | the states the process kept in memory, after the run |
| `peak_memory_bytes`, `materialized_rows` | the evaluation's own footprint |

---

## Diagnostics

Every refusal is one `Diagnostic`: a `kind`, one sentence `message`, and the
places it is about - `location` (`source`, `line`, `column`), `rule`,
`relations`, `function`, and supplementary `detail` (a compiler's report, a
panic payload). Kinds: `parse`, `validation`, `stratification`, `planning`,
`input`, `function`, `evaluation` (a fault on the data, such as division by
zero), `resource` (a limit or a cancellation), `internal` (a defect of the
engine, never a verdict on the program).

The library returns it, the service and the C interface serialize it as JSON,
and the command line prints its rendering on standard error, plus the JSON
object on its own last line when `FLOWLOG_DIAGNOSTIC_JSON=1`. `check` runs
every stage short of evaluation, so a program can be validated where it is
written.

---

## Provenance

Ask for rows to explain (`EvaluationOptions::explain`, the service's `explain`
and `explain_all`, the command line's `--explain`) and the result carries a
`Witness` per row: whether it is present, whether it is an input, the rule
that derives it with its line, and its parents - for an ordinary rule the
matched row of every positive atom of the first body solution, for an
aggregate the solutions of the row's group (those achieving the extremum for
`min`/`max`), with the group's size. This is one-step provenance over the
final states; a parent in a recursive relation is explained by another
request.

---

## Command line

```bash
target/release/executing -p <program.dl> -f <facts_directory> [-c <output>] [options]
```

| option | meaning |
|---|---|
| `-p, --program <FILE>` | the program |
| `-f, --facts <DIR>` | input relation files |
| `-c, --csvs <DIR>` | write outputs to `<DIR>/csvs/<Relation>.csv`, sizes to `size.txt`, counters to `cache-stats.json` |
| `-d, --delimiter <CHAR>` | column delimiter (default `,`) |
| `-w, --workers <N>` | worker threads per evaluation (default 4) |
| `-O <0-3>` | optimization: 1 sideways information passing, 2 structural planning, 3 both |
| `--fat-mode`, `--no-sharing` | heap rows everywhere; no common-subexpression reuse |
| `--check` | parse, validate, stratify and plan; print the report as JSON |
| `--cache-dir <DIR>` | the state cache: read and write states in this directory, evaluating per stratum; without it nothing is cached |
| `--call-cache <DIR>` | compiled blocks |
| `--budget-seconds`, `--memory-limit-mib`, `--tuple-limit` | the evaluation's limits |
| `--explain` | write a witness per output row to `explain.jsonl` |
| `--daemon-socket <PATH>`, `--max-concurrent <N>` | run the service |

An input file holds one row per line, cells separated by the delimiter: a
number as its decimal text, a symbol as its text (which may not contain the
delimiter or a newline). A cell that is not of its column's type refuses the
run naming the file, the cell and its line. A file for an arity-0 relation
holds one line per row and nothing on it. A written relation is a valid input
file, so one run's output can be another run's input.

---

## Versions

- `CALL_ABI_VERSION` (`native_calls.rs`): the call interface; part of every
  compiled block's digest.
- `CACHE_ABI` and `STATE_MAGIC` (`cache.rs`): the cache key and the entry
  format.
- `PROTOCOL_VERSION` (`daemon.rs`): the service protocol, carried in every
  response.

---

## Known limits

- The semiring is chosen at build time (`present-type`, the default, or
  `isize-type`); the observable semantics are set semantics either way.
- A recursive stratum's iteration counter is 16 bits: a fixed point needing
  more than 65,535 rounds is outside this build.
- Fixed-size rows reach `ROW_MAX = 7` columns and key/value halves `KV_MAX =
  4`; wider shapes are planned onto heap rows automatically.
- A function that never returns holds its evaluation; limits are checked
  between operators.
- A witness lists at most 256 body solutions of an aggregate's group.

---

## Datasets

All datasets used in the paper evaluation are publicly available:
[https://huggingface.co/datasets/NemoYuu/flowlog_benchmark/tree/main/dataset/csv](https://huggingface.co/datasets/NemoYuu/flowlog_benchmark/tree/main/dataset/csv)

## Reproducing Paper Figures

This repository includes [FlowLog-Reproduction](https://github.com/HarukiMoriarty/FlowLog-Reproduction) as a git submodule:

```bash
git submodule update --init --recursive
```

## Contributing

Contributions are welcome! Feel free to submit a pull request or open an issue.

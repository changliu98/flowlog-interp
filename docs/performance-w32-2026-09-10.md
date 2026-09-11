# FlowLog Bench performance at 32 workers

The frozen baseline is `fd566eaa7ce86b0da66c31673e4448b1b429134b`, already
using DD 0.25.1 / Timely 0.31.0 and the preceding byte-range/capture fixes.
The benchmark repository is [FlowLog Bench](https://github.com/flowlog-rs/flowlog-bench/tree/2db7c2eab9f64852242a1691b51707f3fb3454ff)
at `2db7c2eab9f64852242a1691b51707f3fb3454ff`.

Four unchanged official interpreter programs run on the retained datasets:
`crdt/crdt`, `tc/G5K-0.001`, `sg/G5K-0.001`, and `andersen/medium`.
Every timed arm uses 32 Timely workers, the same release toolchain and
programs, no optimization flag, and three successful repetitions. No
compilation or profiling runs concurrently with the timed comparisons.

`scripts/benchmark_flowlog_bench.sh` sources FlowLog Bench's interpreter
adapter and measurement helpers. The library CLI no longer emits the
adapter's legacy dataflow timing message, so the wrapper reads complete
process wall time from the adapter's GNU-time sidecars. This includes input
preparation, evaluation, materialization, and teardown; no output CSV is
written during those timings. This timing scope applies to every arm and
must not be compared directly with older dataflow-only benchmark tables.

The baseline profile identified three different bottlenecks:

- TC: approximately 3.0 seconds in dataflow and another 9.5 seconds before
  the result was ready; tree-based materialization accounted for 17.9% of
  sampled CPU time, including its callees.
- Andersen: approximately 9.1 seconds preparing input before the engine's
  execution timer started. The baseline sorted heap-backed rows serially; the first optimization
  moved sorting onto workers but still left a costly merge.
- CRDT: 39.1% of sampled CPU time in syscalls and 19.3% in channel sends.
  Repeated parking and waking dominated its fine-grained execution.

The profiles used a process-local gperftools profiler, extracted under the
ignored artifact directory; host perf permissions and system settings were
left alone. Profiled timings are diagnostic observations, separate from the
unprofiled three-run medians.

| Area | Change |
|---|---|
| Input preparation | Validate and parse byte ranges into compact typed rows; sort/deduplicate on readers; merge sorted runs in parallel; allocate public row vectors after deduplication. |
| Input distribution | Each worker traverses its contiguous share, eliminating the full-input scan on every worker. |
| Worker execution | Use non-parking stepping while an evaluation is active; idle workers still block. Read the fault mutex only when the stop flag is set. |
| Dataflow rows | Fixed-arity rows contain only their 64-bit cells, with checked builders; a two-column row shrinks from 24 to 16 bytes. |
| Capture | Buffer native row types, consolidate locally, and materialize the final signed state after the frontier closes; omit the extra capture arrangement/exchange. |
| Final materialization | Replace the serial BTreeMap with a parallel merge tree; preserve negative weights until support has been combined across workers; avoid re-sorting canonical states. |
| Hashing | Feed SHA-256 in 8 KiB batches without changing the persisted byte format. |
| Cache unions | Reuse a sole contribution by Arc; merge/deduplicate borrowed rows before cloning multi-contribution unions. |
| Row programs and aggregates | Build output rows directly and reduce values through iterators, removing temporary vectors. |
| File output | Format numeric cells directly into the buffered writer instead of allocating a string for every cell. |

Parsing/planning, native-call resolution, disk-cache maintenance, and symbol
handling were also inspected. Planning took only a few milliseconds on this
cohort, native function pointers were already resolved before execution,
and cache cleanup was already bounded. Numeric benchmark results do not
measure string-heavy interning, embedded-function performance, or disk-cache
I/O. The new capture, merge, and row paths are also exercised by the engine's
cache, symbol, native-call, failure, and C-interface tests.

The engine's request/results interfaces, full signed 64-bit value domain,
and persisted state digests remain intact. The low-level reading crate's
row construction now uses `Row::builder().finish()` or `Row::from_slice()`;
`Array` is a read interface. Active workers favor latency over sleeping,
and merge phases may start helper threads while Timely workers are idle.

Reproduce one arm with an isolated result directory:

```sh
WORKERS=32 NUM_RUNS=3 bash scripts/benchmark_flowlog_bench.sh \
  /path/to/flowlog-bench /path/to/frozen-interpreter-programs \
  /path/to/executing /path/to/new-results
```

`target/ultimate-w32/` retains baseline/intermediate/final binaries, source
patches, profiles, test/build logs, frozen program copies, and manifests for
all seven input files (592,785,389 bytes). Per-arm directories retain every
run log, GNU-time sidecar, relation count, and median. Full relation-state
digests are compared with the frozen baseline separately from size checks.

All 12 timed runs succeeded in each arm, with unchanged relation counts.
Separate full-state checks matched **all 31 input/derived relation digests**
in whole-program, cold-cache, and warm-cache execution: 12 program/mode
checks, 93 relation-state comparisons. Every warm check evaluated zero rules. These cached checks validate the
returned states and are separate from the timing table below.

Validation passed **130 Present tests and 131 isize tests**, including
parallel signed merging, zero-arity dataflows, symbols and malformed input,
cache reuse/deletion, native calls, limits, service/C interfaces, packed-row
serialization, and exact compatibility of buffered SHA-256 with the persisted
byte format. Release builds and `git diff --check` passed. The benchmark
repository and its datasets remain unchanged.

Median complete process time and median peak RSS, three runs at 32 workers:

| Workload | Baseline s | Optimized s | Speedup | Baseline MiB | Optimized MiB |
|---|---:|---:|---:|---:|---:|
| crdt/crdt | 12.47 | 4.29 | 2.91x | 520.5 | 475.3 |
| tc/G5K-0.001 | 13.29 | 4.06 | 3.27x | 3985.4 | 4571.4 |
| sg/G5K-0.001 | 16.58 | 6.33 | 2.62x | 4789.4 | 4485.8 |
| andersen/medium | 13.95 | 3.24 | 4.31x | 3748.1 | 2923.6 |

The geometric-mean speedup is **3.22x** on this frozen four-case cohort.
TC trades approximately **14.7% more peak RSS** for its 3.27x speedup; the
other three workloads use less peak memory. Active stepping and parallel
materialization favor latency on a machine with spare cores; these results
do not establish behavior under CPU oversubscription. The full 64-bit cell
domain and complete returned relation states were retained throughout.

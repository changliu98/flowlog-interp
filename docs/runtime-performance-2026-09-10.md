# Runtime performance review, 2026-09-10

The interpreter now uses Differential Dataflow 0.25.1 and Timely 0.31.0,
reads relation files in parallel byte ranges using the configured worker count,
and captures rows in worker-local buffers before publishing each complete batch.
The public engine request API and cache file format are unchanged.

The dependency migration updates scope lifetimes, collection/arrangement
ownership, and recursive feedback handles to the current upstream APIs.
See the [DD release notes](https://github.com/TimelyDataflow/differential-dataflow/releases/tag/differential-dataflow-v0.25.1)
and [Timely changelog](https://github.com/TimelyDataflow/timely-dataflow/blob/master/CHANGELOG.md).

Validation: `cargo test --workspace --locked` passed 125 tests with Present;
`cargo test --workspace --locked --no-default-features --features isize-type`
passed 126 tests. Test builds disabled debug information. Coverage includes
recursive evaluation, aggregation, negation, cache invalidation/reuse, service
and C interfaces, byte boundaries/CRLF/UTF-8/empty input, and buffered signed
retractions across four workers. `git diff --check` also passed.

The timing comparison uses three release binaries with the Present semiring:

- **Baseline:** revision `c495fd5479040ae6cb17761bbf77e4863c61582c`, DD 0.18.0 / Timely 0.25.1.
- **Dependencies:** the dependency/API migration alone, retaining serial input and shared per-record capture locking.
- **Optimized:** the dependency migration plus parallel input and worker-local capture.

All builds use Rust 1.95.0 on Linux x86-64 (kernel 6.8.0-1059-azure,
glibc 2.39), with an affinity mask allowing 128 logical CPUs. Each case records three evaluations at 1, 4,
and 8 workers; configuration order is shuffled with seed 8541. The scan
copies 1,000,000 distinct pairs. Reachability starts with 31,744 edges in
1,024 disjoint chains of 31 edges and derives 507,904 pairs. Expected input
and output digests are computed independently from these definitions.

Timings include file loading, parsing, planning, worker startup, evaluation,
materialization, and cache operations. They exclude formatting/writing output
rows and release compilation. Whole-program runs do not cache; cold runs
create a fresh in-memory cache each repetition; warm runs prime a resident
cache once and then reload identical input files for every measurement.
Warm runs must evaluate zero rules. The OS page cache is not cleared.
The measurements cover these two synthetic workloads on this host.

Reproduce with separately retained `runtime_bench` example binaries:

```sh
python3 scripts/benchmark_runtime.py \
  --output target/maintainer-perf/comparison \
  --engine baseline target/maintainer-perf/baseline-bench \
  --engine dependencies target/maintainer-perf/upgrade-bench \
  --engine optimized target/maintainer-perf/optimized-bench
```

The ignored `target/maintainer-perf/` directory retains the baseline lockfile,
the migration-only source snapshot and patch, build/test logs, and all three
binaries. `comparison/manifest.json` records binary/input hashes and exact run
order; `runs.jsonl` contains individual measurements and row digests;
`summary.json` contains medians. Each source revision should use its own
Cargo target directory when rebuilding these binaries.

All **162/162 measured evaluations** matched the independently computed input
and output digests. Timed warm evaluations all evaluated zero rules.

At 8 workers, cold-cache scan time fell from **0.693 s to 0.329 s (2.11x)**,
and cold-cache reachability from **0.392 s to 0.240 s (1.63x)**. Warm scan
reloads fell from **0.102 s to 0.038 s (2.66x)**. The dependency-only upgrade
had mixed results; the combined reader/capture changes delivered the larger
improvements. Single-worker results were close, including a roughly 4%
slower warm scan in this sample. Millisecond-scale warm reachability timings
should be read with particular caution given the three-repetition sample.

Median evaluation wall time in seconds; ratios above 1 favor the optimized build:

| Workload | Workers | Mode | Baseline | Dependencies | Optimized | Speedup |
|---|---:|---|---:|---:|---:|---:|
| scan | 1 | whole | 0.670015 | 0.670974 | 0.669590 | 1.00x |
| scan | 1 | cold | 0.732901 | 0.705910 | 0.702836 | 1.04x |
| scan | 1 | warm | 0.102442 | 0.099807 | 0.106338 | 0.96x |
| scan | 4 | whole | 0.516840 | 0.566129 | 0.342239 | 1.51x |
| scan | 4 | cold | 0.619193 | 0.609731 | 0.376191 | 1.65x |
| scan | 4 | warm | 0.098727 | 0.098124 | 0.049738 | 1.98x |
| scan | 8 | whole | 0.632046 | 0.666627 | 0.289361 | 2.18x |
| scan | 8 | cold | 0.693231 | 0.669522 | 0.329258 | 2.11x |
| scan | 8 | warm | 0.102027 | 0.098803 | 0.038415 | 2.66x |
| reach | 1 | whole | 0.494032 | 0.478618 | 0.480271 | 1.03x |
| reach | 1 | cold | 0.533968 | 0.490140 | 0.485199 | 1.10x |
| reach | 1 | warm | 0.003040 | 0.002954 | 0.003083 | 0.99x |
| reach | 4 | whole | 0.396223 | 0.370682 | 0.304887 | 1.30x |
| reach | 4 | cold | 0.401645 | 0.412068 | 0.316481 | 1.27x |
| reach | 4 | warm | 0.003195 | 0.003177 | 0.002106 | 1.52x |
| reach | 8 | whole | 0.394619 | 0.359955 | 0.225013 | 1.75x |
| reach | 8 | cold | 0.391848 | 0.390915 | 0.240187 | 1.63x |
| reach | 8 | warm | 0.003303 | 0.003645 | 0.002371 | 1.39x |

# Performance review, 2026-10-31

The stratum cache is removed (every evaluation is one whole-program
dataflow), and an evaluation no longer waits for rustc, a quadratic
validation pass, a second parse, or a profile mutex before it gets to work.
Measured on the FlowLog stages rlcrafter runs and on four paper workloads.

## Workloads

**rlcrafter.** The 16 FlowLog invocations rlcrafter's rulebase2 (RULES.log
head 9697) made while its CLI evaluated eight prepared function inputs,
captured verbatim with their reference outputs by a wrapper installed as
`RLCRAFTER_FLOWLOG_INTERP_BIN` (see `~/research/flowlog-bench/README.md`).
They are two programs, each over eight fact sets:

| program | lines | rules | `.code rust` block | inputs | outputs | strata |
|---|---:|---:|---:|---:|---:|---:|
| s01 (assembly_evidence + 8 stages) | 11241 | 5974 | 1149 lines, 911 functions | 127 relations, 4.3-7.0 MB | 3856 | 415 |
| s02 (wide_evidence + 2 stages) | 6980 | 3544 | 966 lines, 728 functions | 128 relations, 17 KB-1.0 MB | 2206 | 176 |

Every run uses rlcrafter's flags: `-w 2 --fat-mode --profile-json <out>/rule-profile.json --memory-limit-mib 16384`.

**Paper.** `examples/{tc,sg,crdt,andersen}.dl` over the FlowLog Bench
datasets G5K-0.001, crdt and medium, at `-w 4`, with no options.

## Method

`scripts/benchmark_workloads.py` runs every binary on every workload once
per repetition, in a rotating order, on a shared 128-core host whose load
came from other users. It reports wall time, CPU time (including rustc) and
peak RSS of each process, and compares every output CSV and `size.txt` byte
for byte with the captured reference. Both binaries are release builds as
configured in the repository (fat LTO, one codegen unit,
`target-cpu=native`):

- **before**: `f425ebc` (the engine the stages were captured with, sha256 `a94ff77f...`).
- **after**: this revision.

## Results

### rlcrafter stages

Three repetitions of every stage at `-w 2`. All 96 runs wrote outputs byte
for byte identical to the captured references. Medians; RSS is the peak over
the repetitions.

| stage | before wall | after wall | speedup | before CPU | after CPU | before RSS | after RSS |
|---|---:|---:|---:|---:|---:|---:|---:|
| f0020-split-o0-s01 | 7.97 s | 1.69 s | 4.7x | 9.7 s | 4.2 s | 790 MiB | 753 MiB |
| f0029-split-o1-s01 | 7.77 s | 1.39 s | 5.6x | 9.2 s | 3.7 s | 747 MiB | 711 MiB |
| f0056-split-o0-s01 | 8.28 s | 1.84 s | 4.5x | 10.1 s | 4.5 s | 801 MiB | 755 MiB |
| f0059-split-o3-s01 | 9.16 s | 2.40 s | 3.8x | 11.5 s | 5.5 s | 771 MiB | 734 MiB |
| f0102-dd-o2-s01 | 8.29 s | 1.78 s | 4.7x | 10.1 s | 4.3 s | 765 MiB | 714 MiB |
| f0120-dd-o0-s01 | 7.83 s | 1.54 s | 5.1x | 9.4 s | 3.8 s | 794 MiB | 764 MiB |
| f0149-dd-o1-s01 | 8.63 s | 2.19 s | 3.9x | 10.8 s | 5.1 s | 761 MiB | 732 MiB |
| f0159-dd-o3-s01 | 7.98 s | 1.77 s | 4.5x | 9.8 s | 4.3 s | 752 MiB | 718 MiB |
| f0020-split-o0-s02 | 3.62 s | 0.96 s | 3.8x | 4.4 s | 2.3 s | 391 MiB | 388 MiB |
| f0029-split-o1-s02 | 3.61 s | 0.97 s | 3.7x | 4.5 s | 2.2 s | 392 MiB | 390 MiB |
| f0056-split-o0-s02 | 3.68 s | 0.97 s | 3.8x | 4.5 s | 2.2 s | 409 MiB | 404 MiB |
| f0059-split-o3-s02 | 3.83 s | 1.12 s | 3.4x | 4.8 s | 2.6 s | 434 MiB | 426 MiB |
| f0102-dd-o2-s02 | 3.70 s | 1.02 s | 3.6x | 4.6 s | 2.4 s | 401 MiB | 400 MiB |
| f0120-dd-o0-s02 | 3.64 s | 0.96 s | 3.8x | 4.4 s | 2.2 s | 391 MiB | 388 MiB |
| f0149-dd-o1-s02 | 3.78 s | 1.05 s | 3.6x | 4.7 s | 2.4 s | 415 MiB | 413 MiB |
| f0159-dd-o3-s02 | 3.80 s | 1.03 s | 3.7x | 4.7 s | 2.4 s | 402 MiB | 398 MiB |

Geometric means: wall time 4.6x shorter on s01, 3.7x on s02, 4.1x over all
16; CPU time (rustc included) 2.3x and 2.0x lower; peak RSS within 5%.

The after runs' counters (`stats.json`, medians) for s01 and s02: planning
(stratifying, planning, reading the inputs, starting the workers) 0.15 s and
0.07 s; the quick build of the block 0.88 s and 0.66 s, overlapped; building
the dataflow 0.37 s and 0.19 s per worker; the dataflow, including its build
and any wait for the block, 1.37 s and 0.79 s; the whole evaluation 1.56 s
and 0.88 s. `--check` of the s01 program took 3.6 s before and 0.3 s after.

### Paper workloads

`-w 4`, two repetitions (sg: four more, after a first pair that differed by
3.5%); outputs byte-identical. Medians.

| workload | before wall | after wall | before CPU | after CPU | before RSS | after RSS |
|---|---:|---:|---:|---:|---:|---:|
| tc / G5K-0.001 | 11.46 s | 11.33 s | 39.4 s | 39.6 s | 3900 MiB | 3945 MiB |
| sg / G5K-0.001 | 21.92 s | 22.03 s | 80.1 s | 81.4 s | 3863 MiB | 3813 MiB |
| crdt / crdt | 1.39 s | 1.40 s | 5.1 s | 5.2 s | 356 MiB | 367 MiB |
| andersen / medium | 4.65 s | 4.47 s | 14.1 s | 14.4 s | 2680 MiB | 2604 MiB |

These are unchanged within the host's noise. They have no embedded blocks
and small programs, and their time is Differential Dataflow's: on tc about
40% of worker time is in the arrange operator, a third of that in its merge
batcher; crdt's many small iterations are mostly timely's progress tracking.

## Where the time went

Before, on an s01 stage (7.8-9.2 s): validation rebuilt the whole
column-type map for every rule (3856 relations x 5974 rules) and searched
the block's functions linearly at every call site - about 70% of the main
thread's samples - and the
command line parsed and validated the program twice. rustc then built the
block at opt-level 3 with one codegen unit (2.6 s) before the dataflow
could start. The live profile, which rlcrafter always requests, took 15% of
the run: every worker's timely events went through one mutex that the
snapshot writer held while it rendered every operator.

After, on the same stages (1.4-2.4 s): parsing and validation take about
0.15 s; the quick build of the block (0.9 s) runs while the program is
planned (in parallel), the inputs are read and the dataflow is built and
starts; the dataflow finishes 0.2-1.3 s after the block is ready; writing
the 3858 output files takes about 0.14 s.

What remains is Differential Dataflow's per-operator cost on tiny inputs:
the s01 dataflow holds several operators for each of 5974 rules, over a few
thousand rows. In samples of f0059-s01 its execution splits into
arrangement maintenance (39%), progress tracking (24%), allocation (11%),
scheduling (10%), profile logging (9%) and exchange (5%); joins and calls
are about 1% each. On the three stages measured that way (f0020-s01,
f0059-s01, f0059-s02), wall time is the same at `-w 1`, `-w 2` and `-w 4`,
while CPU time and peak memory grow with the worker count (f0059-s01: 5.1,
7.0 and 11.1 s of CPU; 510, 751 and 1226 MiB).

## Tried and not kept

- Skipping the deduplication of non-recursive, non-aggregate heads (sound
  under the Present semiring): 3-10% faster and 12% less memory on most
  stages, but 18% slower on the heaviest one (f0059-s01).
- `--cap-lints allow` for block builds: 5% of the quick build, at the cost
  of the warnings in a failing block's report.
- Removing the stratum cache by itself changes little in whole-program
  runs; it no longer computes the SHA-256 of every relation state, which
  showed on andersen's 34 million input rows (7.5 s to 6.8 s, non-LTO, one
  comparison).

## Reproduce

```sh
cargo build --release --locked -p executing
python3 scripts/benchmark_workloads.py \
  --bin before=/path/to/before/executing --bin after=target/release/executing \
  $(printf -- '--workload %s ' ~/research/flowlog-bench/w/f0*) --workers 2 --reps 3
```

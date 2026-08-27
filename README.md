<p align="center">
  <img src="flowlog_full.png" alt="FlowLog Logo" width="400"/>
</p>

<!-- <h1 align="center">FlowLog</h1> -->

<p align="center">
  <strong>An efficient, scalable, and extensible Datalog engine built atop Differential Dataflow</strong>
</p>

<p align="center">
  <a href="https://arxiv.org/pdf/2511.00865">Paper</a> •
  <a href="#quick-example">Quick Start</a> •
  <a href="#datasets">Datasets</a> •
  <a href="#reproducing-paper-figures">Reproduce Results</a>
</p>

---

## Archive Notice

This repository is a public archive. Active development and maintenance have moved to [this repo](https://github.com/flowlog-rs/FlowLog).

---

## FlowLog Paper

This repo contains the implementation for the paper:

**FlowLog: Efficient and Extensible Datalog via Incrementality**  
Hangdong Zhao, Zhenghong Yu, Srinag Rao, Simon Frisk, Zhiwei Fan, Paraschos Koutris  
VLDB 2026 (Boston)

[**Read the paper on arXiv**](https://arxiv.org/pdf/2511.00865)

---

## FlowLog Architecture

FlowLog uses a modular architecture that collectively creates a Datalog execution pipeline as follows (also see Figure 1 of the paper):

<!-- <p align="center">
  <img src="architecture.png" alt="System Architecture" width="700"/>
</p> -->

```
├── parsing       # Parsing Datalog program
├── strata        # Stratification
├── planning      # Generate logical IR and optimize (per rule)
  ├── catalog       # Generate metadata 
  └── optimizing    # Query optimization 
└── executing     # Executor
  ├── reading       # Reading data from CSV
  └── macros        # Rust macros for code generate each differential operator
```

---

## Quick Example

### Environment Setup
```bash
# Automated setup (recommended):
# The env.sh script automatically handles all requirements including:
# - Rust = 1.89.0 (pinned version for reproducibility)
# - differential-dataflow = 0.16.2 (paper version for reproducibility)
# - timely = 0.23.0 (paper version for reproducibility)
# - ...

# Simply run:
bash tool/env.sh

# After installation, you may need to start a new terminal session
# or run `source ~/.bashrc` (or `source ~/.zshrc` if using zsh)
# so that environment variables and PATH updates take effect.

# Manual verification (optional):
# To check your Rust version after setup
rustc --version  # Should show: rustc 1.89.0
```

> **Note on Versions**: For paper reproducibility, we use differential-dataflow 0.16.2 and timely 0.23.0 as reported in the VLDB paper. However, we are actively maintaining FlowLog and catching up with the most updated versions of these dependencies for improved performance.

### Write a Simple Program

Create a file named `reach.dl` with the following contents. This program computes the set of nodes reachable from the given sources:

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

### Prepare Input Data

Create a directory called `reach` and place the EDB files inside. For this example, you can use [livejournal](https://huggingface.co/datasets/NemoYuu/flowlog_benchmark/blob/main/dataset/csv/livejournal.zip):

```bash
mkdir -p reach
cd reach
curl -LO https://pages.cs.wisc.edu/~m0riarty/dataset/csv/livejournal.zip
unzip livejournal.zip
mv livejournal/* ./
rmdir livejournal
cd ..
```

### Build and Run
```bash
cargo build --release
target/release/executing -p reach.dl -f reach -w 64
```

---

## FlowLog Build

```bash
# Release build
cargo build --release                                             # Batch mode (Present, default)
cargo build --release --features isize-type --no-default-features # Incremental mode (isize)
```

### Execution Modes

FlowLog currently supports two execution modes for Datalog applications:

- **Batch Mode** (default): Uses `differential_dataflow::difference::Present` for static Datalog semantics. This mode only tracks whether facts are present or absent, making it suitable for high-performance static Datalog execution.
- **Incremental Mode**: Uses `isize` as the `diff` type for DD's incremental semantics. This allows tracking how many times each fact is derived, supporting incremental view maintenance for Datalog programs.

#### Build Options

| Execution Mode | Build Command | Use Case |
|----------------|---------------|----------|
| **Batch Mode** (default) | `cargo build --release` | Static Datalog execution (used in the paper benchmarking) |
| **Incremental Mode** | `cargo build --release --features isize-type --no-default-features` | Incremental Datalog execution |


---

## FlowLog Run

After (release) build, use the `executing` binary to run Datalog programs:

```bash
# Basic usage
target/release/executing -p <program.dl> -f <facts_directory> -w <number_threads>

# Example with concrete paths
target/release/executing -p examples/reach.dl -f reach -w 8
```

### Command Options

<table>
<tr>
  <th align="center">Option</th>
  <th align="center">Description</th>
</tr>
<tr>
  <td align="center"><code>-p, --program &lt;FILE&gt;</code></td>
  <td>Path to the Datalog program file (<code>.dl</code> extension)</td>
</tr>
<tr>
  <td align="center"><code>-f, --facts &lt;DIR&gt;</code></td>
  <td>Directory containing input fact files (EDBs)</td>
</tr>
<tr>
  <td align="center"><code>-c, --csvs &lt;DIR&gt;</code></td>
  <td><strong>Optional:</strong> Directory for emitting output results (IDBs). If not set, only print IDB sizes in terminal.</td>
</tr>
<tr>
  <td align="center"><code>-d, --delimiter &lt;CHAR&gt;</code></td>
  <td>Delimiter for input files (default: <code>,</code>)</td>
</tr>
<tr>
  <td align="center"><code>-w, --workers &lt;NUM&gt;</code></td>
  <td>Number of worker threads (default: 1)</td>
</tr>
<tr>
  <td align="center"><code>-O &lt;LEVEL&gt;</code></td>
  <td>Optimization level (0-3): <br>
  <code>0</code> - No optimization <br>
  <code>1</code> - Sideways Information Passing (SIP) <br>
  <code>2</code> - Structural Planning <br>
  <code>3</code> - Both optimizations (SIP + Planning)</td>
</tr>
<tr>
  <td align="center"><code>--call-cache &lt;DIR&gt;</code></td>
  <td>Optional cache directory for native modules compiled from <code>.code rust</code>.</td>
</tr>
</table>

#### Example Commands

```bash
# Basic execution under default settings
target/release/executing -p examples/reach.dl -f reach

# Multi-threaded (16 threads) execution, flushing IDBs to output/
target/release/executing -p examples/tc.dl -f tc -c output -w 16

# Robust execution using both SIP and Planning
target/release/executing -p examples/batik.dl -f batik -d $'\t' -w 32 -O 3

# Debug print RUST_LOG=debug
RUST_LOG=debug target/release/executing -p examples/batik.dl -f batik -c results -O 2
```

### Datasets

All datasets used in the paper evaluation are publicly available:

**Paper Datasets**: [https://huggingface.co/datasets/NemoYuu/flowlog_benchmark/tree/main/dataset/csv](https://huggingface.co/datasets/NemoYuu/flowlog_benchmark/tree/main/dataset/csv)

---

## FlowLog (Datalog) Syntax

FlowLog supports standard Datalog with common extensions:

```datalog
// Simple graph reach
reach(x) :- source(x).
reach(y) :- reach(x), edge(x, y).

// constraints
two_hops(x, z) :- edge(x, y), edge(y, z), x != z.

// negation
indirect_only(x, z) :- edge(x, y), edge(y, z), !edge(x, z).

// aggregation
count_paths(x, z, count(y)) :- edge(x, y), edge(y, z).
max_salary(dept, max(salary)) :- employee(emp_id, salary), works_in(emp_id, dept).
```

### Value domain

A `number` is a 64-bit signed integer, from `-9223372036854775808` to
`9223372036854775807`. Program constants, input cells, row columns, aggregate
results and `@call` arguments and results all live in that one domain.

### Imperative functions in rule bodies

A program can keep row-local imperative logic in the same `.dl` file with one
top-level `.code rust` block. Every public top-level function is callable from
Datalog; private functions, imports, constants, types, and modules remain
implementation details:

```datalog
.code rust
use std::cmp::min;

fn absolute(value: i64) -> i64 {
    value.saturating_abs()
}

pub fn normalize(value: i64, limit: i64) -> i64 {
    min(absolute(value), limit)
}

pub fn acceptable(value: i64) -> bool {
    value % 2 == 0
}
.endcode

.in
.decl Input(value: number)

.printsize
.decl Output(original: number, normalized: number)

.rule
Output(X, Y) :- Input(X), Y = @call(normalize, X, 255), @call(acceptable, Y).
```

An `i64`-returning call binds a number with
`Y = @call(function, arguments...)`. A `bool`-returning call is written bare
and filters out the row when it returns `false`. Calls can consume `i64`
constants, variables from positive relational atoms, and results of earlier
calls. Earlier/later refers to call order in the rule; relational predicates
retain Datalog's unordered meaning.

The current physical ABI deliberately matches FlowLog's row representation:
exports must be safe, synchronous, non-generic free functions whose arguments
are all `i64` and whose result is `i64` or `bool`. Arithmetic/aggregate heads,
using a call result in another relational predicate or ordinary comparison,
text arguments, and call rules with no retained relational column to drive
evaluation are not supported yet. Rules with calls skip SIP rewriting, while
normal structural planning remains available.

Embedded functions have a **purity contract**: they must be deterministic and
must not perform I/O, observe time or randomness, mutate external state, or
otherwise depend on evaluation count or order. Differential Dataflow may run,
repeat, and reorder a call on multiple workers. Loops, local mutation,
conditionals, matching, helper functions, and other ordinary imperative Rust
inside a pure function are fine. A panic is caught at the native boundary and
reported as a worker failure. Embedded Rust is trusted native code and runs
with the same privileges as FlowLog.

The block is compiled directly with `rustc`, which must be available at
runtime. This version supports local code and the Rust standard library, but
not Cargo dependencies. Compilation is content-addressed over the source, call
ABI, and `rustc` version. FlowLog loads one shared library per process and
resolves symbols once per worker, rather than looking them up per tuple. The
cache location is selected in this order: `--call-cache`,
`FLOWLOG_CALL_CACHE`, `$XDG_CACHE_HOME/flowlog/calls`,
`$HOME/.cache/flowlog/calls`, then a temporary directory.

---

## FlowLog Current Limitations (Work In Progress)

**Aggregation**  
FlowLog currently supports `count`, `sum`, `min`, `max` aggregation operators. However, the aggregate field must be the **last argument** in the head IDB. All rules deriving the same IDB must conform to the same **aggregation type** (e.g. `count`, `sum`), and the aggregate must be applied to a single variable. A program that breaks either rule is refused before evaluation, naming the rules that disagree, rather than being evaluated under whichever operator was seen first.

**Rule heads**  
Outside an aggregate, a head argument must be a variable, and a head's arity must match the relation's `.decl`. Head constants and head arithmetic are refused before evaluation rather than being dropped from the projection.

**Compilation**  
FlowLog currently compiles very slowly due to heavy dependencies (e.g., DD/Timely). On r6525 node, a from-scratch release build can take ~16 minutes.

**Arithmetic Head**  
Support for the Arithmetic Head feature is currently unstable and conflicts with the existing SIP optimization. We have therefore moved it to a temporary branch  `nemo_arithmetic`. You can check out this branch to run programs that require this feature (e.g., SSSP). We have confirmed it runs correctly on SSSP, but we do not guarantee correctness in general. On this branch such a program is refused rather than evaluated with the arithmetic dropped, so `examples/sssp.dl` (`min(0)`, `min(d1 + d2)`) runs only on `nemo_arithmetic`.

---

## FlowLog Example Benchmarks

The `examples/` directory contains several sample Datalog programs demonstrating various features and use cases.

---

## Reproducing Paper Figures

This repository includes [FlowLog-Reproduction](https://github.com/HarukiMoriarty/FlowLog-Reproduction) as a git submodule. You can use this submodule to reproduce the experiment figures from the paper. Please initialize submodules after cloning:

```bash
git submodule update --init --recursive
```


---

## Contributing

Contributions are welcome! Feel free to submit a pull request or open an issue.

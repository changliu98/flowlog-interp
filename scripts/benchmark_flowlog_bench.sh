#!/usr/bin/env bash
# Run the retained four-case FlowLog Bench cohort with its interpreter adapter.
# Usage: WORKERS=32 NUM_RUNS=3 bash scripts/benchmark_flowlog_bench.sh \
#        BENCH_ROOT PROGRAM_DIR INTERPRETER_BINARY OUTPUT_DIR
set -euo pipefail
[[ $# == 4 ]] || { echo "BENCH_ROOT PROGRAM_DIR INTERPRETER_BINARY OUTPUT_DIR" >&2; exit 2; }
bench=$(realpath "$1")
INTERPRETER_PROG_DIR=$(realpath -m "$2")
INTERPRETER_BIN=$(realpath "$3")
LOG_DIR=$(realpath -m "$4")
[[ ! -e "$LOG_DIR" ]] || { echo "Output directory already exists: $LOG_DIR" >&2; exit 2; }
WORKERS=${WORKERS:-32}
NUM_RUNS=${NUM_RUNS:-3}
FLOWLOG_RUN_TIMEOUT=${FLOWLOG_RUN_TIMEOUT:-600}
TIME_BIN=${TIME_BIN:-/usr/bin/time}
FACT_DIR="$bench/facts"
INTERPRETER_PROG_URL=https://huggingface.co/datasets/NemoYuu/flowlog_benchmark/resolve/main/program/flowlog_interpreter
source "$bench/scripts/lib/common.sh"
log() { local color="$1" tag="$2"; shift 2; echo "[$tag] $*" >&2; }
die() { echo "$*" >&2; exit 1; }
source "$bench/scripts/engines/interpreter.sh"
for dataset in crdt G5K-0.001 medium; do
    [[ -d "$FACT_DIR/$dataset" ]] || die "Missing FlowLog Bench dataset: $FACT_DIR/$dataset"
done
for program in crdt tc sg andersen; do
    _interpreter_download_program "$program.dl"
done
mkdir -p "$LOG_DIR"

# The library CLI has no legacy "Dataflow executed in" log line. Keep the
# official adapter's commands, timeouts and medians, but read complete process
# wall time from its GNU-time sidecars. Use this same timing scope for all arms.
extract_total_seconds() {
    python3 - "$1.rss" <<'PY'
import re, sys
text = open(sys.argv[1]).read()
elapsed = re.search(r'Elapsed \(wall clock\) time.*: (\S+)', text).group(1)
seconds = 0.0
for component in elapsed.split(':'):
    seconds = seconds * 60 + float(component)
print(f'{seconds:.6f}')
PY
}

python3 - "$bench" "$INTERPRETER_PROG_DIR" "$INTERPRETER_BIN" "$LOG_DIR" "$WORKERS" "$NUM_RUNS" <<'PY'
import hashlib, json, pathlib, platform, subprocess, sys
bench, programs, binary, output = map(pathlib.Path, sys.argv[1:5])
def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1 << 20), b''):
            h.update(block)
    return h.hexdigest()
paths = [bench / 'scripts/engines/interpreter.sh', bench / 'scripts/lib/measure.sh', binary]
paths += [programs / (name + '.dl') for name in ('crdt', 'tc', 'sg', 'andersen')]
for dataset in ('crdt', 'G5K-0.001', 'medium'):
    paths += sorted(p for p in (bench / 'facts' / dataset).iterdir() if p.is_file())
manifest = {
    'bench_revision': subprocess.check_output(['git', '-C', str(bench), 'rev-parse', 'HEAD'], text=True).strip(),
    'host': platform.platform(), 'workers': int(sys.argv[5]), 'runs': int(sys.argv[6]),
    'binary': str(binary), 'sha256': {str(p): digest(p) for p in paths},
    'timing': 'complete process wall time from the official GNU-time sidecars; no optimization or CSV output flags',
}
(output / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
PY

for pair in crdt:crdt tc:G5K-0.001 sg:G5K-0.001 andersen:medium; do
    engine_interpreter_run "${pair%%:*}.dl" "${pair#*:}"
    count=$(cat "$LOG_DIR/${pair%%:*}_${pair#*:}_interpreter.log.n_runs_succeeded")
    [[ "$count" == "$NUM_RUNS" ]] || die "Incomplete benchmark: $pair ($count/$NUM_RUNS)"
done

python3 - "$LOG_DIR" "$NUM_RUNS" <<'PY'
import json, pathlib, re, statistics, sys
root = pathlib.Path(sys.argv[1])
summary = []
for program, dataset in [('crdt', 'crdt'), ('tc', 'G5K-0.001'), ('sg', 'G5K-0.001'), ('andersen', 'medium')]:
    records = []
    for run in range(1, int(sys.argv[2]) + 1):
        path = root / f'{program}_{dataset}_interpreter_run{run}.log'
        sizes = dict((name, int(count)) for name, count in re.findall(r'Size of \[([^]]+)\]: (\d+)', path.read_text()))
        assert sizes, path
        if records:
            assert sizes == records[0]['sizes'], path
        usage = pathlib.Path(str(path) + '.rss').read_text()
        elapsed = re.search(r'Elapsed \(wall clock\) time.*: (\S+)', usage).group(1)
        seconds = 0.0
        for component in elapsed.split(':'):
            seconds = seconds * 60 + float(component)
        rss = int(re.search(r'Maximum resident set size \(kbytes\): (\d+)', usage).group(1))
        records.append({'wall_seconds': seconds, 'rss_kib': rss, 'sizes': sizes})
    summary.append({'program': program, 'dataset': dataset,
                    'wall_seconds': statistics.median(r['wall_seconds'] for r in records),
                    'rss_kib': statistics.median(r['rss_kib'] for r in records), 'runs': records})
(root / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
print(json.dumps(summary, indent=2))
PY

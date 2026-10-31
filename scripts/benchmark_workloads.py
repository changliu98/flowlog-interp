#!/usr/bin/env python3
"""Time engine binaries on captured workloads and byte-compare their outputs.

  benchmark_workloads.py --bin LABEL=PATH [--bin ...] --workload DIR [--workload ...]
                         [--workers N] [--reps R] [--extra "ARGS"] [--out RESULTS.jsonl]

A workload directory holds program.dl and facts/ (a directory or a symlink),
optionally cmd.json (the argv a host ran the engine with; its flags other than
-p/-f/-c/-w are reused, and a --profile-json is pointed into the run's output
directory) and ref/ (a reference -c output directory). Runs are interleaved:
each repetition runs every binary on every workload, in a rotating order, so
a loaded shared machine affects every arm alike.

For each run: wall seconds, user+sys CPU seconds and peak RSS of the process
(and its children), exit status, the run's stats.json when it wrote one, and
whether every csvs/*.csv and size.txt matches the reference byte for byte (the
reference is ref/ when present, else the first binary's first output).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def digest_outputs(directory: Path) -> dict:
    csvs = directory / "csvs"
    result = {}
    if not csvs.is_dir():
        return result
    for path in sorted(csvs.iterdir()):
        if path.suffix == ".csv" or path.name == "size.txt":
            result[path.name] = hashlib.sha256(path.read_bytes()).hexdigest()
    return result


def workload_args(workload: Path, out: Path) -> list:
    """The engine flags the workload was captured with, beyond -p/-f/-c/-w.

    A captured --profile-json is kept and pointed into this run's output
    directory: rlcrafter profiles every run by default.
    """
    cmd = workload / "cmd.json"
    if not cmd.exists():
        return []
    argv = json.loads(cmd.read_text()).get("argv", [])
    extra, skip = [], False
    for arg in argv[1:]:
        if skip:
            skip = False
            continue
        if arg in ("-p", "-f", "-c", "-w", "--program", "--facts", "--csvs", "--workers"):
            skip = True
            continue
        if arg == "--profile-json":
            extra += ["--profile-json", str(out / "rule-profile.json")]
            skip = True
            continue
        extra.append(arg)
    return extra


def run_once(binary: str, workload: Path, workers: int, extra: list, keep: Path | None):
    out = Path(tempfile.mkdtemp(prefix="flbench-", dir=os.environ.get("FLBENCH_TMP", "/tmp")))
    command = [binary, "-p", str(workload / "program.dl"), "-f", str(workload / "facts"),
               "-c", str(out), "-w", str(workers), *workload_args(workload, out), *extra]
    env = dict(os.environ)
    env.setdefault("RUST_LOG", "warn")
    errors = tempfile.TemporaryFile()
    started = time.monotonic()
    proc = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=errors, env=env)
    _, status, usage = os.wait4(proc.pid, 0)
    wall = time.monotonic() - started
    proc.returncode = os.waitstatus_to_exitcode(status)
    errors.seek(0)
    stderr = errors.read()
    cpu = usage.ru_utime + usage.ru_stime
    record = {
        "wall": round(wall, 4), "cpu": round(cpu, 3), "rss_mib": round(usage.ru_maxrss / 1024, 1),
        "status": proc.returncode, "outputs": digest_outputs(out),
        "stderr_tail": stderr.decode(errors="replace")[-600:] if proc.returncode else "",
    }
    stats = out / "csvs" / "stats.json"
    if stats.exists():
        record["stats"] = json.loads(stats.read_text())
    if keep is not None and not keep.exists():
        shutil.copytree(out, keep)
    shutil.rmtree(out, ignore_errors=True)
    return record


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", action="append", required=True)
    parser.add_argument("--workload", action="append", required=True)
    parser.add_argument("--workers", type=int, default=4)
    parser.add_argument("--reps", type=int, default=3)
    parser.add_argument("--extra", default="")
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    binaries = [tuple(spec.split("=", 1)) for spec in args.bin]
    workloads = [Path(w).resolve() for w in args.workload]
    extra = args.extra.split()
    references = {}
    for workload in workloads:
        ref = workload / "ref"
        if ref.is_dir():
            references[workload] = digest_outputs(ref)
    results = {}
    log = args.out.open("a") if args.out else None
    for rep in range(args.reps):
        for wi, workload in enumerate(workloads):
            order = binaries[rep % len(binaries):] + binaries[:rep % len(binaries)]
            for label, binary in order:
                record = run_once(binary, workload, args.workers, extra, None)
                ref = references.get(workload)
                if ref is None:
                    references[workload] = ref = record["outputs"]
                record["match"] = record["outputs"] == ref
                record.update(label=label, workload=workload.name, rep=rep, workers=args.workers)
                results.setdefault((workload.name, label), []).append(record)
                brief = {k: record[k] for k in ("workload", "label", "rep", "wall", "cpu", "rss_mib", "status", "match")}
                if record["status"]:
                    brief["stderr"] = record["stderr_tail"][-300:]
                print(json.dumps(brief), flush=True)
                if log:
                    slim = dict(record)
                    slim.pop("outputs", None)
                    log.write(json.dumps(slim) + "\n")
                    log.flush()
    print("\nsummary (median wall s, median cpu s, max rss MiB, all match):")
    for workload in workloads:
        row = [workload.name]
        for label, _ in binaries:
            runs = results[(workload.name, label)]
            walls = sorted(r["wall"] for r in runs)
            cpus = sorted(r["cpu"] for r in runs)
            row.append(f"{label}: {walls[len(walls)//2]:.3f}s cpu {cpus[len(cpus)//2]:.2f}s "
                       f"rss {max(r['rss_mib'] for r in runs):.0f} "
                       f"{'ok' if all(r['match'] and r['status'] == 0 for r in runs) else 'MISMATCH/FAIL'}")
        print("  " + " | ".join(row))


if __name__ == "__main__":
    main()

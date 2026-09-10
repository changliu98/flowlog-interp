#!/usr/bin/env python3
"""Compare release runtime_bench binaries on identical, independently checked rows."""

import argparse
import hashlib
import itertools
import json
from pathlib import Path
import platform
import random
import statistics
import struct
import subprocess


def prepare(root, rows, chains, length):
    workloads = {}
    for name in ("scan", "reach"):
        directory = root / name
        directory.mkdir()
        relation = "Copy" if name == "scan" else "Reach"
        source = (
            ".in\n.decl Edge(x: number, y: number)\n.input Edge.facts\n"
            f".printsize\n.decl {relation}(x: number, y: number)\n.rule\n"
            f"{relation}(x,y) :- Edge(x,y).\n"
        )
        if name == "reach":
            source += "Reach(x,z) :- Reach(x,y), Edge(y,z).\n"
        (directory / "program.dl").write_text(source)
        with (directory / "Edge.facts").open("w") as out:
            if name == "scan":
                for i in range(rows):
                    out.write(f"{i},{(i * 7919) % rows}\n")
            else:
                for chain in range(chains):
                    for i in range(length):
                        x = chain * (length + 1) + i
                        out.write(f"{x},{x + 1}\n")
        count = rows if name == "scan" else chains * length * (length + 1) // 2
        digest = hashlib.sha256(struct.pack("<QQ", 2, count))
        if name == "scan":
            for i in range(rows):
                digest.update(struct.pack("<qq", i, (i * 7919) % rows))
        else:
            for chain in range(chains):
                base = chain * (length + 1)
                for i in range(length):
                    for j in range(i + 1, length + 1):
                        digest.update(struct.pack("<qq", base + i, base + j))
        input_count = rows if name == "scan" else chains * length
        input_digest = hashlib.sha256(struct.pack("<QQ", 2, input_count))
        if name == "scan":
            input_digest = digest.copy()
        else:
            for chain in range(chains):
                for i in range(length):
                    x = chain * (length + 1) + i
                    input_digest.update(struct.pack("<qq", x, x + 1))
        workloads[name] = {
            "input_rows": input_count,
            "input_sha256": hashlib.sha256((directory / "Edge.facts").read_bytes()).hexdigest(),
            "program_sha256": hashlib.sha256(source.encode()).hexdigest(),
            "expected": {
                relation: {"rows": count, "digest": list(digest.digest())},
                "Edge": {"rows": input_count, "digest": list(input_digest.digest())},
            },
        }
    return workloads


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", nargs=2, action="append", required=True, metavar=("LABEL", "BINARY"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--workers", type=int, nargs="+", default=[1, 4, 8])
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--rows", type=int, default=1_000_000)
    parser.add_argument("--chains", type=int, default=1024)
    parser.add_argument("--length", type=int, default=31)
    parser.add_argument("--seed", type=int, default=8541)
    args = parser.parse_args()
    assert min(args.workers + [args.repetitions, args.rows, args.chains, args.length]) > 0
    binaries = {label: Path(binary).resolve(strict=True) for label, binary in args.engine}
    assert len(binaries) == len(args.engine), "engine labels must be unique"
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    workloads = prepare(root, args.rows, args.chains, args.length)
    cases = list(itertools.product(binaries, workloads, args.workers, ("whole", "cold", "warm")))
    random.Random(args.seed).shuffle(cases)
    manifest = {
        "host": platform.platform(), "seed": args.seed, "repetitions": args.repetitions,
        "parameters": {"rows": args.rows, "chains": args.chains, "length": args.length},
        "binaries": {label: {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                     for label, path in binaries.items()},
        "workloads": workloads, "order": cases,
        "timing": "evaluation wall time includes file input, planning, execution and materialization; excludes result formatting; warm primes the resident cache once; cold uses a fresh engine each repetition; OS page cache is not cleared",
    }
    (root / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    summary = []
    with (root / "runs.jsonl").open("w") as raw:
        for label, name, workers, mode in cases:
            directory = root / name
            tag = f"{label}-{name}-w{workers}-{mode}"
            command = [str(binaries[label]), str(directory / "program.dl"), str(directory),
                       str(workers), mode, str(args.repetitions)]
            with (root / f"{tag}.stderr").open("w") as errors:
                output = subprocess.run(command, text=True, stdout=subprocess.PIPE,
                                        stderr=errors, timeout=300, check=True)
            records = [json.loads(line) for line in output.stdout.splitlines()]
            assert len(records) == args.repetitions, tag
            for record in records:
                assert record["outputs"] == workloads[name]["expected"], (tag, record["outputs"])
                if mode == "warm":
                    assert record["stats"]["rules_evaluated"] == 0, tag
                record.update(engine=label, workload=name)
                raw.write(json.dumps(record) + "\n")
            raw.flush()
            row = {"engine": label, "workload": name, "workers": workers, "mode": mode,
                   "wall_seconds": statistics.median(r["wall_micros"] for r in records) / 1e6,
                   "execution_seconds": statistics.median(r["stats"]["execution_micros"] for r in records) / 1e6,
                   "matching_runs": len(records)}
            summary.append(row)
            print(json.dumps(row), flush=True)
    (root / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")


if __name__ == "__main__":
    main()

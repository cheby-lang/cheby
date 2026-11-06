#!/usr/bin/env python3
"""The run-time reading of `.github/workflows/bench.yml` (D-1035), as Markdown.

Reads the JSON files that `cargo xtask bench-run --against BASE --json FILE` wrote, one per round,
and prints, per group: each round's geometric mean of the per-benchmark ratios (the measured
binary's median over the base's median), the median of the rounds' geometric means, and the lowest
and highest round. A ratio above 1 means slower than the base. A benchmark counts only where the
base printed the measured binary's size and checksum (D-897); the others are listed as left out,
as for a base that predates a benchmark or changed its output. A table per benchmark follows, with
the median, lowest and highest of its rounds' ratios, so one slow benchmark is visible.

This is the computation of the M3 waves' local window report (W-A), so the CI reading and the
local one compare. Standard library only; reads the files in the order given.

Usage: python3 -I bench_rounds.py [--title TEXT] ROUND.json...
"""

import json
import math
import statistics
import sys


def ratios(doc):
    """{(group, name): measured median / base median} for the comparable benchmarks."""
    base = {(r["group"], r["name"]): r for r in doc.get("against", {}).get("results", [])}
    out = {}
    for r in doc.get("results", []):
        b = base.get((r["group"], r["name"]))
        if (
            not b
            or b.get("checksum") != r.get("checksum")
            or b.get("n") != r.get("n")
            or not b.get("median_ns")
        ):
            continue
        out[(r["group"], r["name"])] = r["median_ns"] / b["median_ns"]
    return out


def gmean(xs):
    return math.exp(sum(math.log(x) for x in xs) / len(xs))


def report(docs, title):
    lines = [f"## {title}", ""]
    if not docs:
        return "\n".join(lines + ["No round finished, so there is nothing to compare.", ""])
    meta = docs[0].get("meta", {})
    against = docs[0].get("against", {}).get("cheby", "?")
    lines.append(
        f"{len(docs)} round(s) of `bench-run --against`, {meta.get('runs', '?')} timed runs per "
        f"benchmark and binary; measured `{meta.get('cheby', '?')}` (commit `{meta.get('commit', '?')}`) "
        f"against `{against}`. A ratio above 1 means slower than the base."
    )
    lines.append("")
    per_round = [ratios(doc) for doc in docs]
    groups = sorted({g for rr in per_round for (g, _) in rr})
    head = "| Group | " + " | ".join(f"Round {i}" for i in range(1, len(docs) + 1))
    lines.append(head + " | Median | Lowest | Highest |")
    lines.append("| --- " * (len(docs) + 4) + "|")
    for g in groups:
        gm = []
        for rr in per_round:
            xs = [v for (gg, _), v in rr.items() if gg == g]
            gm.append(gmean(xs) if xs else None)
        known = [x for x in gm if x is not None]
        cells = " | ".join("-" if x is None else f"{x:.3f}" for x in gm)
        lines.append(
            f"| {g} | {cells} | {statistics.median(known):.3f} | {min(known):.3f} | {max(known):.3f} |"
        )
    measured = {(r["group"], r["name"]) for doc in docs for r in doc.get("results", [])}
    left_out = sorted(measured - {k for rr in per_round for k in rr})
    if left_out:
        lines.append("")
        lines.append(
            "Left out (the base could not run them, or printed another size or checksum, D-897): "
            + ", ".join(f"`{g}/{n}`" for g, n in left_out)
        )
    lines += ["", "<details><summary>Per benchmark</summary>", ""]
    lines.append("| Benchmark | Median ratio | Lowest | Highest |")
    lines.append("| --- | --- | --- | --- |")
    for k in sorted({k for rr in per_round for k in rr}):
        xs = [rr[k] for rr in per_round if k in rr]
        lines.append(
            f"| {k[0]}/{k[1]} | {statistics.median(xs):.3f} | {min(xs):.3f} | {max(xs):.3f} |"
        )
    lines += ["", "</details>", ""]
    return "\n".join(lines)


def main(argv):
    title = "Run time against the base"
    files = []
    args = iter(argv)
    for a in args:
        if a == "--title":
            title = next(args, title)
        else:
            files.append(a)
    docs = []
    for path in files:
        with open(path, encoding="utf-8") as f:
            docs.append(json.load(f))
    sys.stdout.write(report(docs, title))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

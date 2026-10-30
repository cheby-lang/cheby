#!/usr/bin/env python3
"""The split of the time outside `main` (D-1254), as Markdown.

Reads the JSON files that `cargo xtask bench-run --split [--against BASE] --json FILE` wrote and
prints, per group and per benchmark, the medians in milliseconds of these buckets, each from the
split's own timed runs on the round's warm cache, after its untimed warm-up:

- `check`: `cheby check`'s wall time: the front end and the constants' evaluation (`check` builds
  and runs its own evaluation module, D-971, D-978, D-1187). On a warm cache it also checks every
  module that holds a constant, and its imports, from source (D-1134).
- outside `main`: `cheby run`'s wall time minus its `main_ms` (CP2's definition of the time outside
  `main`, so the column compares with CP2's inferred one).
- `run − check`: outside `main` minus `check`: compiling, loading, JIT registration and teardown,
  and whatever `run` parses or checks beyond `check`. It is `invalid` for a benchmark whose cache
  log shows `run` parsing or checking more modules than `check` (the subtraction then mixes front
  ends), and it can be negative where `check` does more than `run`.
- the executable's start and stop: the executable's wall time minus its `main_ms`: process start,
  runtime start and stop, without a JIT. A proxy for registration and teardown, never `cheby
  run`'s JIT registration.
- `main`: `main_ms`, which includes the runtime's stop.
- entries − `main` (informative only): `entries_ms` minus `main_ms`, the constants' entries minus
  the runtime's stop.

With a base, each cell also shows HEAD − base for the buckets both binaries have. A base without
`cheby build` (such as `bbed11b`) has no executable, so that bucket is HEAD's alone and is printed
as an upper bound of its share of the growth. A difference is shown only where the base printed
the measured binary's size and checksum (D-897).

Every split run sets `CHEBY_RT_STATS=1`, which turns event counting on (D-768), so the split's
`main` is not the timed runs' `main`. The buckets are medians of separate processes, so they add up
only approximately. Several files (rounds) are pooled: each bucket is the median over every run of
every file.

Standard library only. Usage: python3 -I bench_split.py [--title TEXT] FILE...
Exit 0 when at least one result has a split, 1 when none has, 2 on a usage error.
"""

import json
import os
import statistics
import sys

# (key, column heading) of each bucket, in the tables' order.
BUCKETS = [
    ("check", "`check`"),
    ("outside", "outside `main`"),
    ("run_check", "`run − check`"),
    ("exe", "exe start and stop"),
    ("main", "`main`"),
    ("entries", "entries − `main`"),
]


def median(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def pooled(docs):
    """{role: {(group, name): {"check": [...], "run": [...], "exe": [...] | None, ...}}}."""
    out = {"head": {}, "base": {}}
    for doc in docs:
        for role, results in (
            ("head", doc.get("results", [])),
            ("base", doc.get("against", {}).get("results", [])),
        ):
            for r in results:
                split = r.get("split")
                if not isinstance(split, dict):
                    continue
                key = (r["group"], r["name"])
                entry = out[role].setdefault(
                    key,
                    {
                        "check": [],
                        "run": [],
                        "exe": [],
                        "has_exe": False,
                        "exe_note": None,
                        "logs": [],
                        "printed": (r.get("n"), r.get("checksum")),
                    },
                )
                entry["check"] += [ns / 1e6 for ns in split.get("check_ns", [])]
                entry["run"] += split.get("run", [])
                exe = split.get("exe")
                if isinstance(exe, dict):
                    entry["has_exe"] = True
                    entry["exe"] += exe.get("runs", [])
                elif split.get("exe_note"):
                    entry["exe_note"] = split["exe_note"]
                if isinstance(split.get("cache_log"), dict):
                    entry["logs"].append(split["cache_log"])
    return out


def outside(run):
    if run.get("main_ms") is None or run.get("wall_ns") is None:
        return None
    return run["wall_ns"] / 1e6 - run["main_ms"]


def invalid_reason(logs):
    """Why `run − check` mixes front ends, from the cache logs, or None."""
    for log in logs:
        c, r = log.get("check", {}), log.get("run", {})
        parsed = (r.get("parse", 0) + r.get("parse.std", 0)) - (c.get("parse", 0) + c.get("parse.std", 0))
        checked = r.get("front_miss", 0) - c.get("front_miss", 0)
        if parsed > 0 or checked > 0:
            return f"`run` parsed {max(parsed, 0)} and checked {max(checked, 0)} module(s) more than `check`"
    return None


def buckets(entry):
    """The bucket medians of one binary's pooled split of one benchmark."""
    b = {
        "check": median(entry["check"]),
        "outside": median(outside(r) for r in entry["run"]),
        "main": median(r.get("main_ms") for r in entry["run"]),
        "entries": median(
            r["entries_ms"] - r["main_ms"]
            for r in entry["run"]
            if r.get("entries_ms") is not None and r.get("main_ms") is not None
        ),
        "exe": median(outside(r) for r in entry["exe"]) if entry["has_exe"] else None,
        "invalid": invalid_reason(entry["logs"]),
    }
    b["run_check"] = (
        b["outside"] - b["check"] if b["outside"] is not None and b["check"] is not None else None
    )
    return b


def fmt(value):
    return "-" if value is None else f"{value:.1f}"


def cell(key, head, base, has_base):
    """HEAD's value of bucket `key`, with HEAD − base where both binaries have it."""
    if key == "run_check" and head.get("invalid"):
        return "invalid"
    value = head.get(key)
    if value is None:
        return "-"
    if not has_base:
        return fmt(value)
    if key == "exe" and base is not None and base.get("exe") is None:
        return f"{fmt(value)} (HEAD only, ≤)"
    other = None if base is None else base.get(key)
    if other is None:
        return fmt(value)
    return f"{fmt(value)} ({value - other:+.1f})"


def comparable(head_entry, base_entry):
    return base_entry is not None and base_entry["printed"] == head_entry["printed"]


def render(title, paths, docs):
    data = pooled(docs)
    head, base = data["head"], data["base"]
    if not head:
        return None
    has_base = bool(base)
    meta = docs[0].get("meta", {})
    against = docs[0].get("against", {}).get("cheby")
    rows = {}
    for key, entry in sorted(head.items()):
        b = buckets(entry)
        base_entry = base.get(key)
        bb = buckets(base_entry) if comparable(entry, base_entry) else None
        rows[key] = (b, bb)
    lines = [f"# {title}", ""]
    runs = sorted({len(e["check"]) for e in head.values()})
    lines.append(
        f"{len(paths)} file(s) ({', '.join(os.path.basename(p) for p in paths)}): `{meta.get('cheby', '?')}` "
        f"at `{meta.get('commit', '?')}`"
        + (f" against `{against}`" if against else "")
        + f", on {meta.get('os', '?')}/{meta.get('arch', '?')} ({meta.get('cpu', '?')}); "
        f"{'/'.join(str(r) for r in runs)} timed run(s) per command and benchmark, pooled over the files."
    )
    lines.append("")
    legend = "Medians in milliseconds"
    if has_base:
        legend += "; each cell is HEAD's value and, in parentheses, HEAD − base"
    lines.append(legend + ".")
    lines.append("")
    header = "| {} | n | " + " | ".join(h for _, h in BUCKETS) + " |"
    rule = "| ----- | - | " + " | ".join("-" * len(h) for _, h in BUCKETS) + " |"

    lines += ["## Per group", ""]
    lines.append(
        "Each cell: the median over the group's benchmarks of their medians; with a base, the "
        "median of their differences, over the benchmarks where the base printed the same "
        "output. `run − check` leaves out the benchmarks where it is invalid."
    )
    lines.append("")
    lines.append(header.format("Group"))
    lines.append(rule)
    groups = sorted({g for g, _ in rows})
    for group in groups:
        members = [rows[k] for k in rows if k[0] == group]
        cells = []
        for key, _ in BUCKETS:
            valid = [(b, bb) for b, bb in members if not (key == "run_check" and b.get("invalid"))]
            value = median(b.get(key) for b, _ in valid)
            text = fmt(value)
            bases = [bb for _, bb in valid if bb is not None]
            if has_base and value is not None and bases:
                if key == "exe" and all(bb.get("exe") is None for bb in bases):
                    text += " (HEAD only, ≤)"
                else:
                    diffs = [
                        b[key] - bb[key]
                        for b, bb in valid
                        if bb is not None and b.get(key) is not None and bb.get(key) is not None
                    ]
                    if diffs:
                        text += f" ({median(diffs):+.1f})"
            dropped = len(members) - len(valid)
            if dropped:
                text += f" ({dropped} invalid)"
            cells.append(text)
        lines.append(f"| {group} | {len(members)} | " + " | ".join(cells) + " |")
    lines.append("")

    lines += ["## Per benchmark", ""]
    lines.append(header.format("Benchmark"))
    lines.append(rule)
    for (group, name), (b, bb) in rows.items():
        cells = [cell(key, b, bb, has_base) for key, _ in BUCKETS]
        lines.append(f"| `{group}/{name}` | {len(head[(group, name)]['check'])} | " + " | ".join(cells) + " |")
    lines.append("")

    if has_base:
        lines += ["## The base's own buckets", ""]
        lines.append(header.format("Benchmark"))
        lines.append(rule)
        for key, entry in sorted(base.items()):
            b = buckets(entry)
            cells = [fmt(b.get(k)) if not (k == "run_check" and b.get("invalid")) else "invalid" for k, _ in BUCKETS]
            lines.append(f"| `{key[0]}/{key[1]}` (base) | {len(entry['check'])} | " + " | ".join(cells) + " |")
        lines.append("")

    notes = []
    for (group, name), (b, _) in rows.items():
        if b.get("invalid"):
            notes.append(f"`{group}/{name}`: `run − check` is invalid: {b['invalid']}.")
    for (group, name), entry in sorted(head.items()):
        base_entry = base.get((group, name))
        if base_entry is not None and not comparable(entry, base_entry):
            notes.append(f"`{group}/{name}`: the base printed another size or checksum, so no difference is shown (D-897).")
    exe_notes = sorted({e["exe_note"] for e in base.values() if e["exe_note"]})
    for note in exe_notes:
        notes.append(f"The base has no executable: {note}.")
    for note in sorted({e["exe_note"] for e in head.values() if e["exe_note"]}):
        notes.append(f"HEAD has no executable for some benchmarks: {note}.")
    lines += ["## Notes", ""]
    for note in notes:
        lines.append(f"- {note}")
    lines += [
        "- Every split run sets `CHEBY_RT_STATS=1`, which turns event counting on (D-768), so this "
        "`main` is not the timed runs' `main` (W-A's).",
        "- The buckets are medians of separate processes, so they add up only approximately.",
        "- `check` holds the constants' evaluation (D-971, D-1187) and, on a warm cache, checks every "
        "module that holds a constant from source (D-1134), so `run − check` is biased and can be negative.",
        "- `run − check` holds compiling, loading, JIT registration and teardown together; only phase "
        "timers inside `cheby` (`CHEBY_PHASES`, D-974) would separate JIT registration from compiling. "
        "The executable's start and stop is a proxy for registration and teardown without a JIT.",
    ]
    if has_base and exe_notes:
        lines.append(
            "- Against a base without `cheby build`, the executable's start and stop is HEAD's alone "
            "(`HEAD only, ≤`): an upper bound of that bucket's share of the growth."
        )
    lines.append("")
    return "\n".join(lines)


def main(argv):
    title = "Split of the time outside main (D-1254)"
    args = argv[1:]
    if len(args) >= 2 and args[0] == "--title":
        title, args = args[1], args[2:]
    if not args or any(a.startswith("--") for a in args):
        print(__doc__.strip().splitlines()[-2], file=sys.stderr)
        return 2
    docs = []
    for path in args:
        with open(path, encoding="utf-8") as f:
            docs.append(json.load(f))
    text = render(title, args, docs)
    if text is None:
        print(f"# {title}\n\nNo result has a `split` object (run `bench-run --split`).")
        return 1
    print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))

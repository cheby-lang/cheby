"""Renders `cargo xtask bench-compile --json`'s OQ-P11 readings as Markdown (K7; D-974, D-975,
D-1035, D-1256, D-1266). Standard library only; run it as `python3 -I`.

Usage: python3 -I bench_oqp11.py [--title T] FILE

FILE is the JSON of `bench-compile --oqp11 --restarts` (`oqp11.json`) or of a plain
`bench-compile` (`compile.json`, whose `rows` carry the cold columns). It prints, as present:

- OQ-P11 (D-974): per shape the cold run's time (reported, never budgeted) and, per edit kind
  (`body_edit`, `main_edit`) from the edit rounds' `phases_median` (never the cold run: D-923's
  gate is about changed modules), the median against D-247's 500 ms with its miss (reported, never
  enforced), every phase's share of the median with its rank and whether it is the largest, and
  the three fallback gates' inputs: code generation (`codegen`: lazy compilation, D-923), loading
  (`declare` alone and `declare + define + finalize`: our own loader, D-914, read on `deep`) and
  hashing (`hash`: a stat cache, D-963). The rule CP3 would apply to them is only proposed here
  (GAP(oqp11-gates)); the user decides at CP3 (D-974).
- RB-3's restart causes (D-1256): per row the median, how many runs started again, the first
  attempt's time, the modules checked again and the logged run's parses. A row with no runs, or
  of a cause whose runs did not all start again, is a tool failure upstream and is never rendered
  as a reading.
- The cold columns (D-975, D-1266): a cold `cheby check` and `cheby build` on an empty cache and
  on a cache that std's records fill, and the difference, std's first use inside them.

Exit status: 0 when something was rendered; 1 when the file has no `oqp11`, no `restarts` and no
cold column; 2 on a usage error or a file that is not such JSON (an edit's median or a phase that
is missing, null or not a number among them).
"""

import json
import sys

BUDGET_NS = 500_000_000
# GAP(oqp11-gates): the rule proposed for CP3 (not applied here).
PROPOSED_SHARE = 0.40
CAUSES = ("reader_lowering", "mask", "settings")


class Malformed(Exception):
    """A value the renderer needs is missing or of the wrong kind: the file is not such JSON."""


def usage(message):
    print(f"bench_oqp11.py: {message}", file=sys.stderr)
    print("usage: python3 -I bench_oqp11.py [--title T] FILE", file=sys.stderr)
    sys.exit(2)


def need_ns(value, what):
    """A number of nanoseconds that the file must hold, or `Malformed`."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise Malformed(f"{what} is {json.dumps(value)}, not a number of nanoseconds")
    return int(value)


def secs(ns):
    if ns is None:
        return "-"
    return f"{ns / 1e9:.3f} s"


def pct(part, whole):
    if not whole:
        return "-"
    return f"{100.0 * part / whole:.1f}%"


def miss(median_ns):
    """The median against the budget: by how much it misses, or that it is within."""
    if median_ns > BUDGET_NS:
        return f"misses by {secs(median_ns - BUDGET_NS)}"
    return f"within ({secs(BUDGET_NS - median_ns)} to spare)"


def edit_section(shape, kind, edit, out):
    median_ns = need_ns(edit.get("median_ns"), f"`{shape}`'s {kind} median_ns")
    phases = {
        k: need_ns(v, f"`{shape}`'s {kind} phase `{k}`")
        for k, v in (edit.get("phases_median") or {}).items()
    }
    runs = edit.get("runs_ns") or []
    restarted = edit.get("restarted")
    out.append(
        f"**`{shape}`, {kind.replace('_', ' ')}**: median {secs(median_ns)} of {len(runs)} runs, "
        f"{miss(median_ns)} against D-247's 500 ms (reported, never enforced)"
        + (f"; {restarted} of {len(runs)} runs started again" if restarted else "")
        + "."
    )
    out.append("")
    ranked = sorted(phases.items(), key=lambda kv: (-kv[1], kv[0]))
    largest = ranked[0][0] if ranked and ranked[0][1] > 0 else None
    out.append("| Phase | Median | Share of the median | Rank | Largest |")
    out.append("| ----- | ------ | ------------------- | ---- | ------- |")
    for rank, (name, ns) in enumerate(ranked, start=1):
        out.append(
            f"| `{name}` | {secs(ns)} | {pct(ns, median_ns)} | {rank} | "
            f"{'yes' if name == largest else ''} |"
        )
    out.append("")
    rank_of = {name: rank for rank, (name, _) in enumerate(ranked, start=1)}

    def gate(label, names, note):
        ns = sum(phases.get(n, 0) for n in names)
        joined = " + ".join(f"`{n}`" for n in names)
        is_largest = len(names) == 1 and names[0] == largest
        ranks = ", ".join(f"{n} {rank_of[n]}" for n in names if n in rank_of) or "-"
        out.append(
            f"| {label} | {joined} | {secs(ns)} | {pct(ns, median_ns)} | {ranks} | "
            f"{'yes' if is_largest else ('-' if len(names) > 1 else 'no')} | {note} |"
        )

    out.append("| Fallback gate (input only) | Phases | Median | Share | Rank | Largest | Note |")
    out.append("| -------------------------- | ------ | ------ | ----- | ---- | ------- | ---- |")
    gate("Code generation: lazy compilation (D-923)", ["codegen"], "")
    loading_note = "read on `deep` (D-974)" if shape == "deep" else "D-974 reads it on `deep`"
    gate("Loading: our own loader (D-914)", ["declare"], loading_note)
    gate("Loading: our own loader (D-914)", ["declare", "define", "finalize"], loading_note)
    gate("Hashing: a stat cache (D-963)", ["hash"], "")
    out.append("")


def render_oqp11(o, out):
    lines = o.get("lines")
    rounds = o.get("rounds")
    budget = int(o.get("budget_ns", BUDGET_NS))
    out.append(f"### OQ-P11: edit to run at {lines} lines, {rounds} rounds (D-974)")
    out.append("")
    shapes = o.get("shapes") or {}
    out.append("| Shape | Modules | Cold run (reported, not budgeted) | Body edit, median | Main edit, median |")
    out.append("| ----- | ------- | --------------------------------- | ----------------- | ----------------- |")
    for name, s in shapes.items():
        out.append(
            f"| `{name}` | {s.get('modules', '-')} | {secs(s.get('cold_ns'))} | "
            f"{secs((s.get('body_edit') or {}).get('median_ns'))} | "
            f"{secs((s.get('main_edit') or {}).get('median_ns'))} |"
        )
    out.append("")
    out.append(
        f"Budget: D-247's {secs(budget)} on both shapes, reported and never enforced before M6 "
        "(D-644, D-974). Each median is over the edit rounds' whole-command times; the phases come "
        "from `CHEBY_PHASES` and are exclusive, so their sum is at most the command's time."
    )
    out.append("")
    for name, s in shapes.items():
        for kind in ("body_edit", "main_edit"):
            if s.get(kind):
                edit_section(name, kind, s[kind], out)
    out.append(
        "Proposed for CP3, not applied (GAP(oqp11-gates)): a fallback's gate triggers on a reading "
        f"that misses 500 ms when its phase is the largest of that edit kind's breakdown and at "
        f"least {PROPOSED_SHARE:.0%} of the median (loading: `declare`, or `declare + define + "
        "finalize`, on `deep`). The user decides at CP3 (D-974)."
    )
    out.append("")


def render_restarts(r, out):
    out.append("### RB-3's restart causes (D-1256)")
    out.append("")
    out.append(
        "| Shape | Lines | Command | Cause | Median | Restarted | First attempt | "
        "Modules checked again | Parses | Std parses |"
    )
    out.append(
        "| ----- | ----- | ------- | ----- | ------ | --------- | ------------- | "
        "--------------------- | ------ | ---------- |"
    )
    failures = []
    for row in r.get("rows") or []:
        runs = row.get("runs_ns") or []
        restarted = int(row.get("restarted", 0))
        head = (
            f"| `{row.get('shape')}` | {row.get('lines')} | `{row.get('command')}` | "
            f"`{row.get('cause')}` |"
        )
        if not runs:
            failures.append(f"{row.get('shape')} {row.get('lines')} {row.get('cause')}")
            out.append(f"{head} not a reading: no runs (a tool failure) | | | | | |")
            continue
        if row.get("cause") in CAUSES and restarted < len(runs):
            failures.append(f"{row.get('shape')} {row.get('lines')} {row.get('cause')}")
            out.append(
                f"{head} not a reading: {restarted} of {len(runs)} runs started again "
                "(a tool failure) | | | | | |"
            )
            continue
        out.append(
            f"{head} {secs(row.get('median_ns'))} | {restarted} of {len(runs)} | "
            f"{secs(row.get('first_attempt_ns')) if restarted else '-'} | "
            f"{row.get('rechecked_modules')} of {row.get('modules', '-')} | {row.get('parses')} | "
            f"{row.get('std_parses')} |"
        )
    out.append("")
    out.append(
        "`unedited` and `one_body` are the references; `reader_lowering` (a field only a reader's "
        "lowering reads), `mask` (a package-visible function's borrow mask, D-812) and `settings` "
        "(`CHEBY_POSITIONS=off` after a run with positions on) start the command again. First "
        "attempt: the median of `restart` in `CHEBY_PHASES`. Parses: the untimed logged run's "
        "`parse` and `parse.std` lines (D-1135, D-1263). CP3 picks the fix (D-1256)."
    )
    if failures:
        out.append("")
        out.append("Not readings (a cause's runs did not all start again): " + "; ".join(failures) + ".")
    out.append("")


def render_cold(rows, out):
    out.append("### Cold `check` and `build` (D-975, D-1266)")
    out.append("")
    out.append(
        "| Project | Cold check | Cold check, std's records | Std's first use in the check | "
        "Cold build | Cold build, std's records | Std's first use in the build |"
    )
    out.append(
        "| ------- | ---------- | ------------------------- | ---------------------------- | "
        "---------- | ------------------------- | ----------------------------- |"
    )
    for row in rows:
        label = (
            f"{row['elements']} elements, `{row.get('shape')}`"
            if row.get("elements") is not None
            else f"{row.get('lines')} lines, `{row.get('shape')}`"
        )
        check, check_warm = row.get("cold_check_ns"), row.get("cold_check_std_warm_ns")
        build, build_warm = row.get("cold_build_ns"), row.get("cold_build_std_warm_ns")

        def diff(a, b):
            return secs(a - b) if a is not None and b is not None else "-"

        out.append(
            f"| {label} | {secs(check)} | {secs(check_warm)} | {diff(check, check_warm)} | "
            f"{secs(build)} | {secs(build_warm)} | {diff(build, build_warm)} |"
        )
    out.append("")
    out.append(
        "Each column is the fastest of the row's runs, every run on a fresh cache; \"std's "
        "records\" fills it first with `cheby check` of `small-10000` (seed 2), so std's first use "
        "(D-1266) reads apart. Reported, never enforced (D-975)."
    )
    out.append("")


def main(argv):
    title = None
    args = list(argv)
    if args in (["--help"], ["-h"]):
        print(__doc__)
        return 0
    if len(args) >= 2 and args[0] == "--title":
        title = args[1]
        args = args[2:]
    if len(args) != 1 or args[0].startswith("-"):
        usage("one FILE")
    try:
        with open(args[0], encoding="utf-8") as f:
            data = json.load(f)
    except (OSError, ValueError) as e:
        usage(f"cannot read {args[0]}: {e}")
    if not isinstance(data, dict):
        usage(f"{args[0]} is not a JSON object")
    out = []
    if title:
        out.append(f"## {title}")
        out.append("")
    rendered = False
    try:
        if isinstance(data.get("oqp11"), dict):
            render_oqp11(data["oqp11"], out)
            rendered = True
        if isinstance(data.get("restarts"), dict):
            render_restarts(data["restarts"], out)
            rendered = True
        cold = [r for r in data.get("rows") or [] if isinstance(r, dict) and "cold_check_ns" in r]
        if cold:
            render_cold(cold, out)
            rendered = True
    except (Malformed, TypeError, ValueError, KeyError, AttributeError) as e:
        print(f"bench_oqp11.py: {args[0]} is not such JSON: {e}", file=sys.stderr)
        return 2
    if not rendered:
        print(f"bench_oqp11.py: {args[0]} has no `oqp11`, no `restarts` and no cold column", file=sys.stderr)
        return 1
    print("\n".join(out))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

#!/usr/bin/env python3
"""compare_runs.py -- find where two determinism_probe logs first diverge.

Given two logs written by the determinism_probe mod (two instances started from
the same save, ideally with no input), this reports, per lane, the first sample
whose digest differs -- which is the step the docs/DAY_ONE.md section 4 table
wants ("the step where two runs first differ"). A lane that never differs across
every common sample is reported as matching for that run length.

    python tools/probe/compare_runs.py runA.log runB.log [-o report.md]

Lanes: v=vehicle count, p=vehicle positions (1 m), e=edge geometry (0.1 m),
c=construction list, t=town building counts, m=money per player, n=people count.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

LANES = [
    ("v", "vehicle count"),
    ("p", "vehicle positions (1 m)"),
    ("e", "edge geometry (0.1 m)"),
    ("c", "construction list"),
    ("t", "town building counts"),
    ("m", "money per player"),
    ("n", "people count"),
]

_LINE = re.compile(r"\bstep=(\d+)\b")


def parse_log(path: Path) -> dict[int, dict]:
    """step -> {lane: digest, 'time': str}. Non-sample/comment lines are skipped."""
    samples: dict[int, dict] = {}
    for raw in path.read_text(encoding="utf-8", errors="replace").splitlines():
        if raw.startswith("#") or "step=" not in raw:
            continue
        m = _LINE.search(raw)
        if not m:
            continue
        step = int(m.group(1))
        rec: dict = {}
        for key, _ in LANES:
            km = re.search(r"\b%s=(\S+)" % re.escape(key), raw)
            rec[key] = km.group(1) if km else None
        tm = re.search(r"\btime=(\S+)", raw)
        rec["time"] = tm.group(1) if tm else "?"
        samples[step] = rec
    return samples


def compare(a: dict[int, dict], b: dict[int, dict]) -> dict:
    common = sorted(set(a) & set(b))
    only_a = sorted(set(a) - set(b))
    only_b = sorted(set(b) - set(a))
    lanes: dict[str, dict] = {}
    for key, desc in LANES:
        first_diff = None
        n_err = 0
        n_cmp = 0
        for step in common:
            da, db = a[step].get(key), b[step].get(key)
            if da == "err" or db == "err" or da is None or db is None:
                n_err += 1
                continue
            n_cmp += 1
            if da != db and first_diff is None:
                first_diff = (step, a[step].get("time"), b[step].get("time"), da, db)
        lanes[key] = {"desc": desc, "first_diff": first_diff,
                      "compared": n_cmp, "errors": n_err}
    return {"common": common, "only_a": only_a, "only_b": only_b, "lanes": lanes}


def build_report(a_path: Path, b_path: Path, a: dict, b: dict, res: dict) -> str:
    L: list[str] = []
    w = L.append
    w("# Determinism comparison: `%s` vs `%s`" % (a_path.name, b_path.name))
    w("")
    w("Produced by `tools/probe/compare_runs.py` from two determinism_probe logs.")
    w("")
    w("- samples in A: %d, in B: %d, common steps: %d" % (len(a), len(b), len(res["common"])))
    if res["only_a"]:
        w("- steps only in A: %d (first %s)" % (len(res["only_a"]), res["only_a"][0]))
    if res["only_b"]:
        w("- steps only in B: %d (first %s)" % (len(res["only_b"]), res["only_b"][0]))
    if res["common"]:
        w("- common step range: %d .. %d" % (res["common"][0], res["common"][-1]))
    w("")
    w("## Per-lane first divergence")
    w("")
    w("| lane | description | compared | errors | first differing step | A time | B time |")
    w("|---|---|---|---|---|---|---|")
    any_diff = False
    for key, _ in LANES:
        info = res["lanes"][key]
        fd = info["first_diff"]
        if fd:
            any_diff = True
            step, ta, tb, _da, _db = fd
            w("| `%s` | %s | %d | %d | **%d** | %s | %s |" % (
                key, info["desc"], info["compared"], info["errors"], step, ta, tb))
        else:
            status = "no common samples" if info["compared"] == 0 else "identical"
            w("| `%s` | %s | %d | %d | %s | | |" % (
                key, info["desc"], info["compared"], info["errors"], status))
    w("")
    if any_diff:
        w("## First divergence detail")
        w("")
        for key, _ in LANES:
            fd = res["lanes"][key]["first_diff"]
            if fd:
                step, ta, tb, da, db = fd
                w("- **%s** (%s) first differs at step %d (A t=%s, B t=%s):" % (
                    key, res["lanes"][key]["desc"], step, ta, tb))
                w("  - A: `%s`" % da)
                w("  - B: `%s`" % db)
        w("")
    else:
        w("All lanes with common samples are identical across the compared runs. "
          "For the same PC / same binary this is the expected TPF2 result; for a "
          "cross-platform pair it is the finding docs/DAY_ONE.md section 4 needs.")
        w("")
    # lanes that never produced a comparable value (API absent on this build)
    dead = [k for k, _ in LANES if res["lanes"][k]["compared"] == 0]
    if dead:
        w("> Lanes with no comparable samples (the mod logged `err` -- the API was "
          "absent or failed on this build): %s. Treat these as UNKNOWN, not as "
          "agreement." % ", ".join("`%s`" % k for k in dead))
        w("")
    return "\n".join(L) + "\n"


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(
        description="Report the first per-lane divergence between two determinism_probe logs.")
    ap.add_argument("run_a", type=Path)
    ap.add_argument("run_b", type=Path)
    ap.add_argument("-o", "--output", type=Path, help="write Markdown here (default: stdout)")
    args = ap.parse_args(argv)

    for p in (args.run_a, args.run_b):
        if not p.is_file():
            ap.error("no such file: %s" % p)
    a, b = parse_log(args.run_a), parse_log(args.run_b)
    if not a or not b:
        print("compare_runs: one of the logs has no parseable samples "
              "(A=%d, B=%d)" % (len(a), len(b)), file=sys.stderr)
        return 2
    res = compare(a, b)
    report = build_report(args.run_a, args.run_b, a, b, res)
    if args.output:
        args.output.write_text(report, encoding="utf-8")
        print("wrote %s" % args.output, file=sys.stderr)
    else:
        sys.stdout.write(report)
    # exit 1 if any lane diverged, so a CI/soak harness notices
    return 1 if any(res["lanes"][k]["first_diff"] for k, _ in LANES) else 0


if __name__ == "__main__":
    raise SystemExit(main())

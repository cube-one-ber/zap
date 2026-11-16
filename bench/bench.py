#!/usr/bin/env python3
"""Median wall and CPU time of zap commands, baseline build vs candidate.

Usage: bench.py BASELINE CANDIDATE [--cases FILE] [--runs N] [--min-speedup X]
Exits non-zero when any command's wall-time speedup falls below --min-speedup.
"""
import argparse, os, resource, shlex, statistics, subprocess, sys, time

parser = argparse.ArgumentParser()
parser.add_argument("baseline")
parser.add_argument("candidate")
parser.add_argument("--cases", default=os.path.join(os.path.dirname(__file__), "cases.txt"))
parser.add_argument("--runs", type=int, default=7)
parser.add_argument("--min-speedup", type=float, default=None)
opts = parser.parse_args()

ENV = {"COLUMNS": "120", "PATH": "/usr/bin", "HOME": os.environ.get("HOME", "/tmp")}

def sample(cmd):
    """(wall ms, cpu ms) of one run."""
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    start = time.perf_counter()
    subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=ENV)
    wall = (time.perf_counter() - start) * 1000
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    cpu = (after.ru_utime - before.ru_utime + after.ru_stime - before.ru_stime) * 1000
    return wall, cpu

def median(binary, args):
    sample([binary] + args)  # warm the page cache
    runs = [sample([binary] + args) for _ in range(opts.runs)]
    return statistics.median(w for w, _ in runs), statistics.median(c for _, c in runs)

cases = [l.strip() for l in open(opts.cases) if l.strip() and not l.startswith("#")]
print(f"{'command':40} {'base wall':>9} {'cand wall':>9} {'speedup':>8} {'base cpu':>9} {'cand cpu':>9} {'speedup':>8}")
slow = []
for case in cases:
    args = shlex.split(case)
    b, c = median(opts.baseline, args), median(opts.candidate, args)
    speedup = b[0] / max(c[0], 0.01)
    print(f"{case:40} {b[0]:9.1f} {c[0]:9.1f} {speedup:7.1f}x {b[1]:9.1f} {c[1]:9.1f} {b[1] / max(c[1], 0.1):7.1f}x")
    if opts.min_speedup is not None and speedup < opts.min_speedup:
        slow.append(f"{case} ({speedup:.2f}x)")
if slow:
    print(f"\nslower than {opts.min_speedup}x the baseline: " + ", ".join(slow), file=sys.stderr)
    sys.exit(1)

#!/usr/bin/env python3
"""Run pacman's pactest suite against zap's engine and compare with pacman.

Each test runs in its own pactest process (and temporary root), in parallel,
inside a user namespace so scriptlets can chroot without fakechroot.

  run.py --pacman-src DIR --bin target/release/zap-pactest [--baseline /usr/bin/pacman]
         [--expect pactest/expected.txt] [--write-expect FILE] [-j N] [TEST...]

Results per test: pass, fail, or unsupported (the shim refused the
operation). With --expect, exits non-zero when a test listed there as
passing no longer passes.
"""
import argparse, concurrent.futures, glob, os, re, shutil, subprocess, sys

def run_one(pactest, binary, test):
    proc = subprocess.run(
        [sys.executable, pactest, "--pacman", binary, "--keep-root", test],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, cwd=os.path.dirname(pactest))
    out = proc.stdout
    root = re.search(r"pacman testing root saved: (\S+)", out)
    unsupported = False
    log = ""
    if root:
        logfile = os.path.join(root.group(1), "var/log/pactest.log")
        if os.path.exists(logfile):
            log = open(logfile, errors="replace").read()
            unsupported = "zap-pactest: unsupported operation" in log
        shutil.rmtree(root.group(1), ignore_errors=True)
    if unsupported:
        return "unsupported", out, log
    if re.search(r"^ok \d+ ", out, re.M) and not re.search(r"^not ok", out, re.M):
        return "pass", out, log
    if re.search(r"^ok \d+ .*# SKIP", out, re.M) or "1..0 # SKIP" in out:
        return "skip", out, log
    return "fail", out, log

def run_all(pactest, binary, tests, jobs):
    with concurrent.futures.ThreadPoolExecutor(jobs) as pool:
        return dict(zip(tests, pool.map(lambda t: run_one(pactest, binary, t), tests)))

def main():
    p = argparse.ArgumentParser()
    p.add_argument("--pacman-src", required=True)
    p.add_argument("--bin", required=True)
    p.add_argument("--baseline")
    p.add_argument("--expect")
    p.add_argument("--write-expect")
    p.add_argument("-j", "--jobs", type=int, default=os.cpu_count())
    p.add_argument("-v", "--verbose", action="store_true")
    p.add_argument("tests", nargs="*")
    o = p.parse_args()
    if os.geteuid() != 0:
        # Root in a user namespace: real chroot for scriptlets and hooks.
        os.execvp("unshare", ["unshare", "--map-root-user", "--map-auto", sys.executable] + sys.argv)
    pactest = os.path.join(os.path.abspath(o.pacman_src), "test/pacman/pactest.py")
    testdir = os.path.join(os.path.abspath(o.pacman_src), "test/pacman/tests")
    names = o.tests or sorted(os.path.basename(t)[:-3] for t in glob.glob(testdir + "/*.py"))
    tests = [os.path.join(testdir, n if n.endswith(".py") else n + ".py") for n in names]
    binary = os.path.abspath(o.bin)
    ours = run_all(pactest, binary, tests, o.jobs)
    theirs = run_all(pactest, o.baseline, tests, o.jobs) if o.baseline else {}
    counts = {}
    for t in tests:
        name = os.path.basename(t)[:-3]
        status, out, log = ours[t]
        counts[status] = counts.get(status, 0) + 1
        line = f"{status:12} {name}"
        if theirs:
            line += f"  (pacman: {theirs[t][0]})"
        if status != "unsupported" and (not theirs or status != theirs[t][0]):
            print(line)
            if o.verbose:
                print("\n".join("    " + l for l in (out + log).splitlines()[-40:]))
    print(" ".join(f"{k}={v}" for k, v in sorted(counts.items())))
    if theirs:
        better = sum(1 for t in tests if ours[t][0] == "pass" and theirs[t][0] != "pass")
        worse = sum(1 for t in tests if ours[t][0] != "pass" and theirs[t][0] == "pass" and ours[t][0] != "unsupported")
        print(f"vs pacman: {worse} regressions, {better} only zap passes")
    if o.write_expect:
        with open(o.write_expect, "w") as f:
            for t in tests:
                if ours[t][0] == "pass":
                    f.write(os.path.basename(t)[:-3] + "\n")
    if o.expect:
        expected = {l.strip() for l in open(o.expect) if l.strip() and not l.startswith("#")}
        lost = sorted(n for n in expected if ours.get(os.path.join(testdir, n + ".py"), ("missing",))[0] != "pass")
        if lost:
            print("expected to pass but did not: " + " ".join(lost))
            sys.exit(1)

main()

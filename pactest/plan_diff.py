#!/usr/bin/env python3
"""Compare transaction plans from zap's engine with `pacman --print` on this
system's real databases.

The local and sync databases are copied into a temporary dbpath (and the
root is a temporary directory), so neither program can touch the system.
Each case runs `pacman <args>` and `zap-pactest <args>` with the same
expanded pacman.conf and compares their output and exit status.

Usage: plan_diff.py --bin target/release/zap-pactest [--samples N] [--seed S]
"""
import argparse
import os
import random
import shutil
import subprocess
import sys
import tempfile


def run(cmd):
    p = subprocess.run(cmd, capture_output=True, text=True, env={**os.environ, "LC_ALL": "C"})
    return p.stdout + p.stderr + f"exit {p.returncode}\n"


def names(cmd):
    return subprocess.run(cmd, capture_output=True, text=True, check=True).stdout.split()


def cases(rng, samples):
    """Plans worth comparing: installs with dependency resolution, the full
    upgrade, and removals with and without recursion or cascading."""
    available = sorted(set(names(["pacman", "-Slq"])) - set(names(["pacman", "-Qq"])))
    installed = names(["pacman", "-Qq"])
    yield ["-Sup"]
    yield ["-Sup", "--ignore", ",".join(rng.sample(installed, min(5, len(installed))))]
    for pkg in rng.sample(available, min(samples, len(available))):
        yield ["-Sp", pkg]
    for _ in range(samples // 4):
        yield ["-Sp", *rng.sample(available, 3)]
    for pkg in rng.sample(installed, min(samples, len(installed))):
        yield ["-Sp", "--needed", pkg]
        yield ["-Rp", pkg]
        yield ["-Rsp", pkg]
        yield ["-Rcp", pkg]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--samples", type=int, default=100)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--pacman", default="/usr/bin/pacman")
    args = ap.parse_args()
    zap = os.path.abspath(args.bin)

    with tempfile.TemporaryDirectory(prefix="plan-diff-") as tmp:
        dbpath = os.path.join(tmp, "db")
        root = os.path.join(tmp, "root")
        cache = os.path.join(tmp, "cache")
        os.makedirs(root)
        os.makedirs(cache)
        shutil.copytree("/var/lib/pacman/local", os.path.join(dbpath, "local"))
        # Only the databases; pacman's download staging dirs are not readable.
        shutil.copytree("/var/lib/pacman/sync", os.path.join(dbpath, "sync"),
                        ignore=lambda d, entries: [e for e in entries if e.startswith("download-")])
        conf = os.path.join(tmp, "pacman.conf")
        with open(conf, "w") as f:
            f.write(subprocess.run(["pacman-conf"], capture_output=True, text=True, check=True).stdout)
        common = ["--config", conf, "--root", root, "--dbpath", dbpath, "--cachedir", cache, "--noconfirm"]

        failed = total = 0
        for case in cases(random.Random(args.seed), args.samples):
            total += 1
            want = run([args.pacman, *common, *case])
            got = run([zap, *common, *case])
            if want != got:
                failed += 1
                print(f"DIFF: {' '.join(case)}")
                for line in list(__import__("difflib").unified_diff(want.splitlines(), got.splitlines(), "pacman", "zap", lineterm=""))[:20]:
                    print("    " + line)
        print(f"{total - failed}/{total} plans equal")
        sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()

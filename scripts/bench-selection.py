#!/usr/bin/env python3
"""Interleave CLI builds against commit diffs on one unchanged app checkout.

CPU and peak RSS are measured for each child separately with wait4; git and
the harness are outside the measured interval. No checkout or install is done.
"""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import tempfile
import time


def git(root, *args):
    """Run an unmeasured Git query in the app checkout."""
    return subprocess.check_output(["git", "-C", str(root), *args], text=True).strip()


def run(binary, root, anchors, diff, base, timeout=300):
    """Measure one child, rejecting hangs and incomplete anchor responses."""
    command = [str(binary), "--root", str(root), "--diff", str(diff),
               "--base", base, "--granularity", "symbol", "--only", "downstream", "--json"]
    for anchor in anchors:
        command.extend(["--anchor", anchor])
    with tempfile.TemporaryFile() as output, tempfile.TemporaryFile() as errors:
        start = time.perf_counter()
        child = subprocess.Popen(command, cwd=root, stdout=output, stderr=errors)
        try:
            deadline = start + timeout
            while True:
                pid, status, usage = os.wait4(child.pid, os.WNOHANG)
                if pid:
                    break
                remaining = deadline - time.perf_counter()
                if remaining <= 0:
                    child.kill()
                    _, status, usage = os.wait4(child.pid, 0)
                    child.returncode = os.waitstatus_to_exitcode(status)
                    raise TimeoutError(f"{binary} exceeded {timeout}s")
                time.sleep(min(0.01, remaining))
        except BaseException:
            if child.returncode is None:
                child.kill()
                child.wait()
            raise
        wall = time.perf_counter() - start
        child.returncode = os.waitstatus_to_exitcode(status)
        errors.seek(0)
        if child.returncode:
            raise RuntimeError(f"{binary} exited {child.returncode}: {errors.read().decode()}")
        output.seek(0)
        answers = json.load(output)["anchors"]
        selection = {answer["anchor"]: answer["affected"] for answer in answers}
        if len(answers) != len(anchors) or set(selection) != set(anchors):
            raise RuntimeError(f"{binary} did not answer every anchor")
        rss_bytes = usage.ru_maxrss * (1 if platform.system() == "Darwin" else 1024)
        return selection, {"cpu_s": usage.ru_utime + usage.ru_stime,
                           "wall_s": wall, "peak_rss_bytes": rss_bytes}


def main():
    """Compare interleaved builds and write a report only complete on success."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, type=Path)
    parser.add_argument("--cases", required=True, type=Path, help="JSON array with source fields")
    parser.add_argument("--anchor-prefix", default="apps/mobile")
    parser.add_argument("--binary", action="append", required=True, metavar="LABEL=PATH")
    parser.add_argument("--commit", action="append", required=True)
    parser.add_argument("--runs", type=int, default=15)
    parser.add_argument("--timeout", type=float, default=300,
                        help="maximum seconds per measured child (default: 300)")
    parser.add_argument("--require-equal-selection", action="store_true",
                        help="fail if any build selects different anchors from the first")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if args.runs < 1:
        parser.error("--runs must be positive")
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout must be finite and positive")
    binaries = {}
    for entry in args.binary:
        label, separator, path = entry.partition("=")
        if not separator or not label or label in binaries:
            parser.error("each binary must have a unique LABEL=PATH")
        binaries[label] = Path(path).resolve(strict=True)
    root = args.root.resolve(strict=True)
    anchors = list(dict.fromkeys(str(Path(args.anchor_prefix) / case["source"])
                                for case in json.loads(args.cases.read_text())))
    if not anchors:
        parser.error("cases must contain at least one anchor")
    commits = [git(root, "rev-parse", f"{commit}^{{commit}}") for commit in args.commit]
    report = {"complete": False, "app_head": git(root, "rev-parse", "HEAD"),
              "app_status": git(root, "status", "--porcelain"),
              "platform": platform.platform(), "anchors": anchors,
              "binaries": {label: str(path) for label, path in binaries.items()},
              "binary_sha256": {label: hashlib.sha256(path.read_bytes()).hexdigest()
                                for label, path in binaries.items()},
              "runs": args.runs, "timeout_s": args.timeout, "commits": {}}
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    labels = list(binaries)
    with tempfile.TemporaryDirectory(prefix="fallout-bench-") as scratch:
        diff = Path(scratch) / "change.diff"
        for commit in commits:
            base = git(root, "rev-parse", f"{commit}^1")
            diff.write_bytes(subprocess.check_output([
                "git", "-C", str(root), "diff", "--no-ext-diff", "--no-color", "-U3", base, commit]))
            results = {}
            # Warm every build and establish its expected selection separately:
            # fixes between releases may legitimately change the selection.
            for label, binary in binaries.items():
                selection, _ = run(binary, root, anchors, diff, base, args.timeout)
                results[label] = {"selection": selection, "samples": []}
            for repetition in range(args.runs):
                # Rotate the first build to distribute drift and warm-cache effects.
                order = labels[repetition % len(labels):] + labels[:repetition % len(labels)]
                for label in order:
                    selection, sample = run(binaries[label], root, anchors, diff, base, args.timeout)
                    if selection != results[label]["selection"]:
                        changed = {anchor: [results[label]["selection"].get(anchor), affected]
                                   for anchor, affected in selection.items()
                                   if results[label]["selection"].get(anchor) != affected}
                        raise RuntimeError(f"unstable selection: {commit} {label}: {changed}")
                    results[label]["samples"].append(sample)
                if (git(root, "rev-parse", "HEAD") != report["app_head"]
                        or git(root, "status", "--porcelain") != report["app_status"]):
                    raise RuntimeError("app checkout changed during the benchmark")
            baseline = {anchor for anchor, affected in results[labels[0]]["selection"].items() if affected}
            for label, result in results.items():
                result["medians"] = {key: statistics.median(sample[key] for sample in result["samples"])
                                     for key in ("cpu_s", "wall_s", "peak_rss_bytes")}
                selected = {anchor for anchor, affected in result["selection"].items() if affected}
                result["added_vs_first"] = sorted(selected - baseline)
                result["removed_vs_first"] = sorted(baseline - selected)
                if args.require_equal_selection and selected != baseline:
                    raise RuntimeError(f"selection differs: {commit} {label}: "
                                       f"added {result['added_vs_first']}, removed {result['removed_vs_first']}")
                median = result["medians"]
                print(f"{commit[:10]} {label}: CPU {median['cpu_s']:.4f}s, "
                      f"wall {median['wall_s']:.4f}s, RSS {median['peak_rss_bytes'] / 1024**2:.2f} MiB; "
                      f"selection +{len(selected - baseline)}/-{len(baseline - selected)}", flush=True)
            report["commits"][commit] = {"base": base, "results": results}
            args.output.write_text(json.dumps(report, indent=2) + "\n")
    for label, path in binaries.items():
        if hashlib.sha256(path.read_bytes()).hexdigest() != report["binary_sha256"][label]:
            raise RuntimeError(f"binary changed during the benchmark: {label}")
    report["complete"] = True
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()

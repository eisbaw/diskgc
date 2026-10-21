#!/usr/bin/env python3
"""Small agent pre-job gate: decide from diskgc's exit code and JSON report.

Example: ./docs/agent-pre-job.py --min-free 50G --timeout 10m --select \
    'profile:cargo-target' --roots /work /work

Only decision=start permits the job to begin. A scheduler can retry
retry_later, add capacity for defer_no_space, and alert on abort_error.
"""

import argparse
import json
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--min-free", required=True)
    parser.add_argument("--target-free")
    parser.add_argument("--timeout", default="10m")
    parser.add_argument("--config")
    parser.add_argument("--select")
    parser.add_argument("--roots", action="append", metavar="DIR")
    parser.add_argument("path", help="filesystem whose available space is checked")
    args = parser.parse_args()

    command = ["diskgc", "ensure", "--json", "--min-free", args.min_free,
               "--timeout", args.timeout]
    if args.target_free:
        command.extend(["--target-free", args.target_free])
    if args.config:
        command.extend(["--config", args.config])
    if args.select:
        command.extend(["--select", args.select])
    if args.roots:
        for root in args.roots:
            command.extend(["--roots", root])
    command.append(args.path)

    try:
        run = subprocess.run(command, stdout=subprocess.PIPE, text=True,
                             check=False)
        report = json.loads(run.stdout)
        if not isinstance(report, dict):
            raise ValueError("report is not an object")
        expected = {0: "met", 1: "exhausted", 3: "error", 75: "lock_timeout"}
        if run.returncode not in expected:
            raise ValueError(f"unsupported diskgc exit {run.returncode}: {report.get('status')}")
        if report.get("status") != expected[run.returncode]:
            raise ValueError("exit code and JSON status disagree")
        after = report.get("avail_after_bytes")
        minimum = report.get("min_free_bytes")
        target = report.get("target_free_bytes")
        met = report.get("target_met")
        if (not isinstance(minimum, int) or isinstance(minimum, bool)
                or not isinstance(target, int) or isinstance(target, bool)
                or (after is None and (run.returncode != 3 or met is not None))
                or (after is not None and (
                    not isinstance(after, int) or isinstance(after, bool)
                    or not isinstance(met, bool) or met != (after >= target)
                    or (run.returncode == 0 and after < minimum)
                    or (run.returncode in (1, 75) and after >= minimum)))):
            raise ValueError("report has inconsistent space fields")
        for key in ("actions", "skipped", "errors"):
            if not isinstance(report.get(key), list):
                raise ValueError(f"report has no {key} list")
        if run.returncode == 3 and not report["errors"]:
            raise ValueError("error exit has no JSON errors")
        if run.returncode == 75 and report["actions"]:
            raise ValueError("lock timeout reported actions")
    except (OSError, ValueError) as error:
        print(json.dumps({"decision": "abort_invalid_report", "error": str(error)}))
        return 4

    decision = {0: "start", 1: "defer_no_space", 3: "abort_error",
                75: "retry_later"}[run.returncode]
    print(json.dumps({"decision": decision, "status": report["status"],
                      "available_bytes": after, "min_free_bytes": minimum,
                      "target_met": met,
                      "skipped": report["skipped"], "errors": report["errors"]},
                     separators=(",", ":")))
    return run.returncode


if __name__ == "__main__":
    sys.exit(main())

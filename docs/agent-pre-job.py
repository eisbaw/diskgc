#!/usr/bin/env python3
"""Small agent pre-job gate: decide from diskgc's exit code and JSON report.

Example: ./docs/agent-pre-job.py --min-free 50G --timeout 10m --select \
    'profile:cargo-target && filter:idle-1d' --roots /work /work

Keep an idle filter in the selection: parallel agents build between each
other's runs, and without one a pre-job reclaims a sibling's target seconds
after its build finished (diskgc keeps only targets a visible process is
using right now).

Prints one JSON line with a decision, and exits with diskgc's exit code
(4 when the report itself is unusable). Only decision=start permits the job
to begin. A scheduler can:
  start               0    run the job
  defer_no_space      1    add capacity: everything eligible was reclaimed
  retry_later         75   another reclaim held the host lock to the deadline
                      124  the deadline passed while reclaiming
  abort_provisioning  2    the host is not provisioned (JSON cause
                           `provisioning`, e.g. no /run/diskgc/lock): alert ops
  abort_config        2    this call or its configuration is wrong (cause
                           parse, config, usage or safety): fix the job
  abort_error         3    a reclaim or host operation failed: alert
  abort_interrupted   130, 143  diskgc was stopped by SIGINT / SIGTERM
  abort_invalid_report  4  no report, or one that contradicts its exit code
"""

import argparse
import json
import subprocess
import sys

# diskgc's exit code -> the JSON status it must carry (docs/reference.md,
# Exit codes). Any other code is not diskgc's (126/127: not started).
STATUS = {0: "met", 1: "exhausted", 2: "usage_error", 3: "error",
          75: "lock_timeout", 124: "deadline", 130: "interrupted",
          143: "interrupted"}


def is_int(value):
    return isinstance(value, int) and not isinstance(value, bool)


def check_space(code, report):
    """The space fields of a run that measured: 0, 1, 3, 75 and 124."""
    after = report.get("avail_after_bytes")
    minimum = report.get("min_free_bytes")
    target = report.get("target_free_bytes")
    met = report.get("target_met")
    if not is_int(minimum) or not is_int(target):
        # A run error before the first sample (exit 3 at startup) measured
        # nothing, so a percentage goal has no bytes yet; anything else
        # without a goal is not a report diskgc writes.
        nothing_measured = (report.get("avail_before_bytes") is None
                            and after is None and met is None)
        if code == 3 and nothing_measured:
            return
        raise ValueError("report has no space goal")
    if after is None:
        # Only a run error may lack its final sample, and then nothing is met.
        if code != 3 or met is not None:
            raise ValueError("report has no final space sample")
        return
    if (not is_int(after) or not isinstance(met, bool)
            or met != (after >= target)
            or (code == 0 and after < minimum)
            or (code in (1, 75, 124) and after >= minimum)):
        raise ValueError("report has inconsistent space fields")


def check(code, report):
    """Raise ValueError unless the report is what exit `code` promises."""
    if not isinstance(report, dict):
        raise ValueError("report is not an object")
    if code not in STATUS:
        raise ValueError(f"exit {code} is not one of diskgc's")
    if report.get("status") != STATUS[code]:
        raise ValueError(f"exit {code} and JSON status {report.get('status')!r} disagree")
    for key in ("actions", "skipped", "errors"):
        if not isinstance(report.get(key), list):
            raise ValueError(f"report has no {key} list")
    if code in (2, 3) and not report["errors"]:
        raise ValueError(f"exit {code} has no JSON errors")
    if code == 2:
        if report["actions"]:
            raise ValueError("a refused run reported actions")
        if not all(isinstance(e, dict) and isinstance(e.get("cause"), str)
                   for e in report["errors"]):
            raise ValueError("an error has no cause")
    elif code in (0, 1, 3, 75, 124):
        check_space(code, report)
    if code == 75 and report["actions"]:
        raise ValueError("lock timeout reported actions")


def decide(code, report):
    if code == 2:
        causes = {error["cause"] for error in report["errors"]}
        return "abort_provisioning" if "provisioning" in causes else "abort_config"
    return {0: "start", 1: "defer_no_space", 3: "abort_error",
            75: "retry_later", 124: "retry_later",
            130: "abort_interrupted", 143: "abort_interrupted"}[code]


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
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
    for root in args.roots or []:
        command.extend(["--roots", root])
    command.append(args.path)

    try:
        run = subprocess.run(command, stdout=subprocess.PIPE, text=True,
                             check=False)
        report = json.loads(run.stdout)
        check(run.returncode, report)
    except (OSError, ValueError) as error:
        print(json.dumps({"decision": "abort_invalid_report", "error": str(error)}))
        return 4

    print(json.dumps({"decision": decide(run.returncode, report),
                      "status": report["status"],
                      "available_bytes": report.get("avail_after_bytes"),
                      "min_free_bytes": report.get("min_free_bytes"),
                      "target_met": report.get("target_met"),
                      "skipped": report["skipped"], "errors": report["errors"]},
                     separators=(",", ":")))
    return run.returncode


if __name__ == "__main__":
    sys.exit(main())

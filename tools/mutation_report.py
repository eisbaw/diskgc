"""Validate cargo-mutants results against an exact, successful selection."""

from collections import Counter
import json
from pathlib import Path


SUMMARY_COUNTS = {
    "CaughtMutant": "caught",
    "MissedMutant": "missed",
    "Timeout": "timeout",
    "Unviable": "unviable",
}


def validate_report(reports: Path, selected: list[str], *, require_caught: bool = True) -> dict[str, int]:
    mutants = json.loads((reports / "mutants.json").read_text())
    outcomes = json.loads((reports / "outcomes.json").read_text())
    if len(selected) != len(set(selected)):
        raise ValueError("selection contains duplicate mutants")
    listed = [entry["name"] for entry in mutants]
    if Counter(listed) != Counter(selected):
        raise ValueError("report mutants differ from selection")
    entries = outcomes["outcomes"]
    baselines = [entry for entry in entries if entry.get("scenario") == "Baseline"]
    if len(baselines) != 1 or baselines[0].get("summary") != "Success":
        raise ValueError("mutation baseline did not succeed exactly once")
    if len(entries) != len(selected) + 1:
        raise ValueError("outcome count differs from selection plus baseline")
    actual = {}
    counted: Counter[str] = Counter()
    for entry in entries:
        scenario = entry["scenario"]
        if scenario == "Baseline":
            continue
        if not isinstance(scenario, dict) or set(scenario) != {"Mutant"}:
            raise ValueError("unknown mutation scenario")
        name = scenario["Mutant"]["name"]
        summary = entry["summary"]
        if name in actual or summary not in SUMMARY_COUNTS:
            raise ValueError(f"duplicate mutant or unknown summary: {name}: {summary}")
        actual[name] = summary
        counted[SUMMARY_COUNTS[summary]] += 1
    if set(actual) != set(selected) or type(outcomes["total_mutants"]) is not int or outcomes["total_mutants"] != len(selected):
        raise ValueError("reported outcomes do not cover every selected mutant")
    counts = {}
    for category in SUMMARY_COUNTS.values():
        value = outcomes[category]
        if type(value) is not int or value < 0 or value != counted[category]:
            raise ValueError(f"{category} count differs from outcome summaries")
        counts[category] = value
    survivors = [f"{summary}: {name}" for name, summary in actual.items() if summary != "CaughtMutant"]
    if survivors and require_caught:
        raise ValueError("non-caught mutants require review and follow-up: " + "; ".join(survivors))
    return counts

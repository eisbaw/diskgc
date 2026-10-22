"""The pre-task size history must survive interrupted writes and stay bounded."""

import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from scratch_tally import MAX_HISTORY_BYTES, MAX_HISTORY_RECORDS, read_records, record_tally


def record(value):
    return {
        "scratch_bytes": value,
        "legacy_tmp_bytes": value,
        "root_target_bytes": value,
        "available_bytes": value,
    }


class ScratchHistoryTests(unittest.TestCase):
    def test_corrupt_tail_keeps_previous_valid_tally(self):
        stream = io.BytesIO((json.dumps(record(7)) + "\n{incomplete").encode())
        rows, corrupt = read_records(stream)
        self.assertEqual(list(rows), [record(7)])
        self.assertEqual(corrupt, 1)

    def test_only_latest_1024_tallies_are_retained(self):
        stream = io.BytesIO(
            b"".join((json.dumps(record(i)) + "\n").encode() for i in range(1100))
        )
        rows, corrupt = read_records(stream)
        self.assertEqual(corrupt, 0)
        self.assertEqual(len(rows), MAX_HISTORY_RECORDS)
        self.assertEqual(rows[0], record(76))
        self.assertEqual(rows[-1], record(1099))

    def test_large_history_reads_only_its_tail(self):
        stream = io.BytesIO(
            b"x" * (MAX_HISTORY_BYTES + 17)
            + b"\n"
            + (json.dumps(record(9)) + "\n").encode()
        )
        rows, corrupt = read_records(stream)
        self.assertEqual(list(rows), [record(9)])
        self.assertEqual(corrupt, 0)

    def test_failed_replacement_keeps_previous_history(self):
        with tempfile.TemporaryDirectory() as root:
            history = Path(root) / "scratch-tallies.jsonl"
            history.write_text(json.dumps(record(7)) + "\n")
            with (
                patch("scratch_tally.SCRATCH", Path(root)),
                patch("scratch_tally.HISTORY", history),
                patch("scratch_tally.HISTORY_LOCK", Path(root) / "scratch-tallies.lock"),
                patch("scratch_tally.os.replace", side_effect=OSError("interrupted")),
            ):
                with self.assertRaisesRegex(OSError, "interrupted"):
                    record_tally(record(8))
            self.assertEqual(history.read_text(), json.dumps(record(7)) + "\n")


if __name__ == "__main__":
    unittest.main()

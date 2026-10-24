import unittest

import retro


class RetroTest(unittest.TestCase):
    def test_a_note_with_retro_lines_is_one_row_with_its_cycle_time(self):
        log = (
            "abc1234\ngate: FAST | exit=0\ntask: TASK-143\nstarted: 2026-10-24T09:12Z  # briefed\n"
            "merged: 2026-10-24T11:42:00Z\nreview_rounds: 0\nwalked: yes\n\x1e\n"
        )
        [row] = list(retro.rows(log))
        self.assertEqual(row[:2], ["abc1234", "TASK-143"])
        self.assertEqual(row[2], "2026-10-24T09:12Z")
        self.assertEqual(row[-1], "2.5")
        self.assertEqual(row[KEYS_INDEX["found_by"]], "-")

    def test_notes_without_a_task_line_and_empty_notes_are_left_out(self):
        log = "aaa\ngate: E2E | exit=0\nqa: pass 2026-10-24T10:00Z\n\x1e\nbbb\n\n\x1e\n"
        self.assertEqual(list(retro.rows(log)), [])

    def test_an_unparseable_time_gives_no_cycle_time(self):
        [row] = list(retro.rows("ccc\ntask: TASK-1\nstarted: yesterday\nmerged: 2026-10-24T11:40Z\n\x1e"))
        self.assertEqual(row[-1], "-")


KEYS_INDEX = {k: i + 1 for i, k in enumerate(retro.KEYS)}

if __name__ == "__main__":
    unittest.main()

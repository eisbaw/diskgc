"""Tests for tools/e2e_selfcheck.py and tools/nix_dead.py: what counts as
left behind, and the GC estimate's arithmetic and its refusal to make
Nix unlink another program's registrations (Podman and Nix are not run)."""

import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import e2e_selfcheck  # noqa: E402
import nix_dead  # noqa: E402

BEFORE = {
    "containers": ["diskgc-e2e-1-kept-earlier"],
    "volumes": ["v1"],
    "images": ["localhost/diskgc-e2e-base:old"],
    "tmp_system": [],
    "tmp": ["nix-build-1-old"],
    "runtime": [],
    "worktree": ["?? notes.txt"],
}


class Leftovers(unittest.TestCase):
    def test_an_unchanged_host_is_clean(self):
        self.assertEqual(e2e_selfcheck.leftovers(BEFORE, BEFORE, set()), {})

    def test_whatever_is_new_is_reported_by_kind(self):
        after = {kind: list(entries) for kind, entries in BEFORE.items()}
        after["containers"].append("diskgc-e2e-2-new")
        after["volumes"].append("v2")
        after["tmp_system"].append("tmp.leaked")
        after["runtime"].append("vm-state-machine")
        after["worktree"].append("?? result")
        self.assertEqual(
            e2e_selfcheck.leftovers(BEFORE, after, set()),
            {
                "containers": ["diskgc-e2e-2-new"],
                "volumes": ["v2"],
                "tmp_system": ["tmp.leaked"],
                "runtime": ["vm-state-machine"],
                "worktree": ["?? result"],
            },
        )

    def test_a_new_image_is_fine_only_when_retained(self):
        after = dict(BEFORE, images=["localhost/diskgc-e2e-base:old", "localhost/diskgc-e2e-base:new"])
        self.assertEqual(
            e2e_selfcheck.leftovers(BEFORE, after, set()), {"images": ["localhost/diskgc-e2e-base:new"]}
        )
        self.assertEqual(e2e_selfcheck.leftovers(BEFORE, after, {"localhost/diskgc-e2e-base:new"}), {})

    def test_gone_entries_are_not_leftovers(self):
        after = dict(BEFORE, containers=[], images=[])
        self.assertEqual(e2e_selfcheck.leftovers(BEFORE, after, set()), {})


class ScratchNames(unittest.TestCase):
    def test_only_harness_shaped_names_count_and_lock_files_are_retained(self):
        with tempfile.TemporaryDirectory() as d:
            for name in [
                "diskgc-x",
                "nix-build-9-1",
                "vm-state-machine",
                "disk.qcow2",
                "diskgc-e2e-host-pid.lock",
                "diskgc",
                "pipewire-0",
                "nix-shell.abc",
            ]:
                Path(d, name).touch()
            self.assertEqual(
                e2e_selfcheck.scratch_names(d, e2e_selfcheck.SCRATCH_NAMES, e2e_selfcheck.RETAINED_RUNTIME),
                ["disk.qcow2", "diskgc-x", "nix-build-9-1", "vm-state-machine"],
            )
        self.assertEqual(e2e_selfcheck.scratch_names("/nonexistent-t311", ["*"]), [])

    def test_runner_refs_come_from_this_checkouts_runner_links(self):
        with tempfile.TemporaryDirectory() as d:
            runner = Path(d) / "store-runner"
            (runner / "share" / "diskgc-e2e").mkdir(parents=True)
            (runner / "share" / "diskgc-e2e" / "image-ref").write_text("localhost/diskgc-e2e-base:new\n")
            (Path(d) / ".gcroots").mkdir()
            os.symlink(runner, Path(d) / ".gcroots" / "e2e-runner")
            os.symlink("/nonexistent", Path(d) / ".gcroots" / "dev")
            self.assertEqual(e2e_selfcheck.runner_refs(Path(d)), {"localhost/diskgc-e2e-base:new"})


class NixDead(unittest.TestCase):
    def test_nar_sizes_add_up_and_invalid_entries_are_counted(self):
        infos = [{"/nix/store/a": {"narSize": 3}, "/nix/store/b.lock": None}, {"/nix/store/c": {"narSize": 4}}]
        self.assertEqual(nix_dead.nar_total(infos), (7, 2, 1))

    def test_dangling_registrations_are_found_and_split_by_owner(self):
        with tempfile.TemporaryDirectory() as d:
            auto, repo = Path(d, "auto"), Path(d, "repo")
            auto.mkdir()
            (repo / ".gcroots").mkdir(parents=True)
            os.symlink(repo / ".gcroots" / "live", repo / ".gcroots" / "live-target")
            (repo / ".gcroots" / "live").touch()
            os.symlink(repo / ".gcroots" / "live", auto / "a")
            os.symlink(repo / ".gcroots" / "e2e-runner-release-01", auto / "b")
            os.symlink(Path(d, "elsewhere", "result"), auto / "c")
            dangling, unreadable = nix_dead.dangling_registrations(auto)
            self.assertEqual(sorted(Path(t).name for t in dangling), ["e2e-runner-release-01", "result"])
            self.assertEqual(unreadable, [])
            self.assertEqual(
                [Path(t).name for t in nix_dead.foreign(dangling, repo)],
                ["result"],
            )
            self.assertEqual(nix_dead.foreign([str(repo) + "-sibling/x"], repo), [str(repo) + "-sibling/x"])


if __name__ == "__main__":
    unittest.main()

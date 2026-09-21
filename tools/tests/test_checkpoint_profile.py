import importlib.util
import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "checkpoint_profile", ROOT / "tools" / "checkpoint-profile.py"
)
profile = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(profile)


class CheckpointProfileTest(unittest.TestCase):
    def test_measured_interval_excludes_setup_and_sums_each_phase(self):
        setup = b"checkpoint_phase lsn=1 phase=manifest slot=-1 elapsed_us=9 block_gets=0 block_puts=0 deleted=0\n"
        measured = (
            b"checkpoint_phase lsn=2 phase=row_sst slot=0 elapsed_us=10 block_gets=1 block_puts=2 deleted=0\n"
            b"checkpoint_phase lsn=2 phase=row_sst slot=1 elapsed_us=12 block_gets=3 block_puts=4 deleted=0\n"
            b"checkpoint_phase lsn=2 phase=gc_delete slot=-1 elapsed_us=20 block_gets=0 block_puts=0 deleted=5\n"
        )
        result = profile.parse(setup + measured, len(setup))
        self.assertEqual(len(result["events"]), 3)
        self.assertEqual(result["events"][0]["slot"], 0)
        self.assertEqual(result["events"][2]["slot"], None)
        self.assertEqual(
            result["totals"]["row_sst"],
            {"events": 2, "elapsed_us": 22, "block_gets": 4, "block_puts": 6, "deleted": 0},
        )
        self.assertEqual(result["totals"]["gc_delete"]["deleted"], 5)

    def test_missing_measurements_fail(self):
        with self.assertRaisesRegex(ValueError, "wrong checkpoint phase fields"):
            profile.parse(b"checkpoint_phase lsn=1 phase=manifest\n", 0)
        with self.assertRaisesRegex(ValueError, "no checkpoint phase lines"):
            profile.parse(b"server started\n", 0)

    def test_request_window_rejects_a_decreasing_counter(self):
        before = {"schema_version": 1, "requests": {
            "delete": 1, "get": 2, "list": 3, "put": 4, "range_get": 5}}
        after = {"schema_version": 1, "requests": {
            "delete": 2, "get": 2, "list": 3, "put": 6, "range_get": 5}}
        self.assertEqual(profile.request_delta(before, after)["put"], 2)
        after["requests"]["delete"] = 0
        with self.assertRaisesRegex(ValueError, "delete counter decreased"):
            profile.request_delta(before, after)

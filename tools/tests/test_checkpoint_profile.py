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
        setup = b"checkpoint_phase lsn=1 phase=manifest slot=-1 started_ns=1000 ended_ns=10000 elapsed_us=9 block_gets=0 block_puts=0 deleted=0\n"
        measured = (
            b"checkpoint_phase lsn=2 phase=row_sst_delta slot=0 started_ns=20000 ended_ns=30000 elapsed_us=10 block_gets=1 block_puts=2 deleted=0\n"
            b"checkpoint_phase lsn=2 phase=row_sst_full slot=1 started_ns=30000 ended_ns=42000 elapsed_us=12 block_gets=3 block_puts=4 deleted=0\n"
            b"checkpoint_phase lsn=2 phase=gc_delete slot=-1 started_ns=42000 ended_ns=62000 elapsed_us=20 block_gets=0 block_puts=0 deleted=5\n"
        )
        result = profile.parse(setup + measured, len(setup))
        self.assertEqual(len(result["events"]), 3)
        self.assertEqual(result["events"][0]["slot"], 0)
        self.assertEqual(result["events"][2]["slot"], None)
        self.assertEqual(
            result["totals"]["row_sst_delta"],
            {"events": 1, "elapsed_us": 10, "block_gets": 1, "block_puts": 2, "deleted": 0},
        )
        self.assertEqual(
            result["totals"]["row_sst_full"],
            {"events": 1, "elapsed_us": 12, "block_gets": 3, "block_puts": 4, "deleted": 0},
        )
        self.assertEqual(result["totals"]["gc_delete"]["deleted"], 5)

    def test_missing_measurements_fail(self):
        with self.assertRaisesRegex(ValueError, "wrong checkpoint phase fields"):
            profile.parse(b"checkpoint_phase lsn=1 phase=manifest\n", 0)
        with self.assertRaisesRegex(ValueError, "no checkpoint phase lines"):
            profile.parse(b"server started\n", 0)

    def test_correlates_operation_latency_with_each_overlapping_phase(self):
        events = [
            {"phase": "row_sst", "started_ns": 100, "ended_ns": 300},
            {"phase": "gc_delete", "started_ns": 400, "ended_ns": 500},
        ]
        operations = [
            {"worker": 0, "operation": 0, "kind": "write", "started_ns": 50, "ended_ns": 150},
            {"worker": 1, "operation": 0, "kind": "read", "started_ns": 200, "ended_ns": 450},
            {"worker": 0, "operation": 1, "kind": "read", "started_ns": 600, "ended_ns": 650},
        ]
        result = profile.correlate(events, operations)
        self.assertEqual(result["clock"], "CLOCK_REALTIME")
        self.assertEqual(result["by_phase"]["row_sst"]["operations"], 2)
        self.assertEqual(result["by_phase"]["row_sst"]["p99_ms"], 0.00025)
        self.assertEqual(result["by_phase"]["row_sst"]["operation_intersection_ms"], 0.00015)
        self.assertEqual(result["by_phase"]["gc_delete"]["operations"], 1)
        self.assertEqual(result["outside_phases"]["operations"], 1)
        self.assertEqual(result["profiled_phase_intersection_ms"], 0.0002)

    def test_request_window_rejects_a_decreasing_counter(self):
        before = {"schema_version": 1, "requests": {
            "delete": 1, "get": 2, "list": 3, "put": 4, "range_get": 5}}
        after = {"schema_version": 1, "requests": {
            "delete": 2, "get": 2, "list": 3, "put": 6, "range_get": 5}}
        self.assertEqual(profile.request_delta(before, after)["put"], 2)
        after["requests"]["delete"] = 0
        with self.assertRaisesRegex(ValueError, "delete counter decreased"):
            profile.request_delta(before, after)

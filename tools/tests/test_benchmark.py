import importlib.util
import pathlib
import threading
import types
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("benchmark", ROOT / "tools" / "benchmark.py")
benchmark = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(benchmark)


class BenchmarkTest(unittest.TestCase):
    def test_nearest_rank_percentiles(self):
        values = [1_000_000, 2_000_000, 3_000_000, 4_000_000, 5_000_000]
        self.assertEqual(benchmark.percentile(values, 50), 3.0)
        self.assertEqual(benchmark.percentile(values, 95), 5.0)

    def test_object_metrics_are_subtracted_by_operation(self):
        before = {
            "schema_version": 1,
            "requests": {"delete": 1, "get": 2, "list": 3, "put": 4, "range_get": 5},
            "request_body_bytes": 10,
            "response_body_bytes": 20,
            "errors": 1,
        }
        after = {
            "schema_version": 1,
            "requests": {"delete": 2, "get": 4, "list": 6, "put": 8, "range_get": 10},
            "request_body_bytes": 30,
            "response_body_bytes": 50,
            "errors": 2,
        }
        self.assertEqual(
            benchmark.subtract_metrics(after, before),
            {
                "schema_version": 1,
                "requests": {"delete": 1, "get": 2, "list": 3, "put": 4, "range_get": 5},
                "request_body_bytes": 20,
                "response_body_bytes": 30,
                "errors": 1,
            },
        )

    def test_access_path_counters_are_subtracted(self):
        before = {
            "sequential_scans": 2,
            "sequential_tuples_read": 20,
            "index_scans": 3,
            "index_tuples_fetched": 3,
        }
        after = {
            "sequential_scans": 2,
            "sequential_tuples_read": 20,
            "index_scans": 11,
            "index_tuples_fetched": 11,
        }
        self.assertEqual(
            benchmark.subtract_access_path(after, before),
            {
                "index_scans": 8,
                "index_tuples_fetched": 8,
                "sequential_scans": 0,
                "sequential_tuples_read": 0,
            },
        )

    def test_tail_range_is_bounded_at_the_high_end_of_the_index(self):
        self.assertEqual(
            benchmark.workload_sql("tail-range", 7, 19, 1000),
            "SELECT sum(payload), count(*) FROM benchmark_kv WHERE id >= 969",
        )
        self.assertEqual(
            benchmark.workload_sql("tail-range", 0, 0, 8),
            "SELECT sum(payload), count(*) FROM benchmark_kv WHERE id >= 1",
        )

    def test_insert_workload_leaves_the_fixed_row_body_at_its_default(self):
        self.assertEqual(
            benchmark.workload_sql("insert", 2, 7, 1000),
            "INSERT INTO benchmark_kv(id, payload) VALUES (2001008, 0)",
        )

    def test_validation_enforces_required_index_access(self):
        result = {
            "workload": {"require_index": True},
            "results": {
                "attempted_operations": 4,
                "completed_operations": 4,
                "errors": [],
                "latency_ms": {
                    "minimum": 1,
                    "p50": 1,
                    "p95": 1,
                    "p99": 1,
                    "maximum": 1,
                },
                "fixed_memory_occupancy": None,
                "object_store": None,
                "access_path": {
                    "index_scans": 3,
                    "index_tuples_fetched": 3,
                    "sequential_scans": 1,
                    "sequential_tuples_read": 4,
                },
            },
        }
        failures = benchmark.validate(result)
        self.assertIn("workload did not execute an index scan per operation", failures)
        self.assertIn("workload unexpectedly executed sequential scans", failures)

    def test_validation_rejects_missing_work_and_unordered_latency(self):
        result = {
            "results": {
                "attempted_operations": 2,
                "completed_operations": 1,
                "errors": [],
                "latency_ms": {
                    "minimum": 1,
                    "p50": 3,
                    "p95": 2,
                    "p99": 4,
                    "maximum": 5,
                },
                "fixed_memory_occupancy": None,
                "object_store": None,
            }
        }
        failures = benchmark.validate(result)
        self.assertIn("not every attempted operation completed", failures)
        self.assertIn("latency percentiles are absent or unordered", failures)

    def test_validation_rejects_the_ungrouped_two_put_commit_shape(self):
        result = {
            "label": "concurrent-update",
            "workload": {"name": "update", "synchronized": True, "clients": 4},
            "results": {
                "attempted_operations": 4,
                "completed_operations": 4,
                "errors": [],
                "latency_ms": {
                    "minimum": 1,
                    "p50": 1,
                    "p95": 1,
                    "p99": 1,
                    "maximum": 1,
                },
                "fixed_memory_occupancy": 0.5,
                "object_store": {
                    "requests": {
                        "delete": 0,
                        "get": 0,
                        "list": 0,
                        "put": 8,
                        "range_get": 0,
                    }
                },
            },
        }
        self.assertIn(
            "concurrent commit PUT amplification exceeded the regression bound",
            benchmark.validate(result),
        )

    def test_synchronized_worker_failure_aborts_peer_barriers(self):
        original = benchmark.PgConnection

        class Connection:
            created = 0

            def __init__(self, *_args):
                self.identifier = Connection.created
                Connection.created += 1

            def close(self):
                pass

            def query(self, sql):
                if sql == "SELECT version()":
                    return [["PostgreSQL 18 test double"]]
                if self.identifier == 2 and sql.startswith("UPDATE benchmark_kv"):
                    raise RuntimeError("injected worker failure")
                return []

        args = types.SimpleNamespace(
            targets=[],
            host="127.0.0.1",
            port=5432,
            user="postgres",
            database="postgres",
            setup=False,
            rows=8,
            clients=4,
            operations=2,
            synchronized=True,
            require_index=False,
            workload="update",
            maintenance_interval=0.0,
            object_metrics=None,
            pid=None,
            fixed_memory_bytes=None,
            label="barrier-failure",
        )
        outcome = []
        benchmark.PgConnection = Connection
        try:
            runner = threading.Thread(target=lambda: outcome.append(benchmark.run(args)), daemon=True)
            runner.start()
            runner.join(1)
            self.assertFalse(runner.is_alive(), "peer workers remained blocked at the barrier")
        finally:
            benchmark.PgConnection = original
        self.assertTrue(outcome)
        self.assertTrue(outcome[0]["results"]["errors"])


if __name__ == "__main__":
    unittest.main()

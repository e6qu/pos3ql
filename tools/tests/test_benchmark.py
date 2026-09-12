import importlib.util
import pathlib
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


if __name__ == "__main__":
    unittest.main()

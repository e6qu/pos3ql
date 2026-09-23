import json
import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "tools" / "benchmark-matrix-report.py"


def measured(throughput, p99, requests=None):
    return {
        "completed_operations": 10,
        "throughput_ops_per_second": throughput,
        "latency_ms": {"p99": p99, "maximum": p99 * 2},
        "object_store": requests,
        "fixed_memory_plan_bytes": 1048576,
        "errors": [],
    }


class BenchmarkMatrixReportTest(unittest.TestCase):
    def write_run(self, root, backend, binary="binary-sha"):
        directory = root / backend
        directory.mkdir()
        request_metrics = backend == "fixture"
        environment = {
            "schema_version": 1,
            "artifact_type": "environment",
            "git_commit": "commit",
            "pos3ql_binary_sha256": binary,
            "logical_cpu_count": 2,
            "suite": {
                "mode": "checkpoint",
                "rows": 100,
                "table_capacity": 200,
                "operations_per_client": 10,
                "clients": 2,
                "logical_replicas": 0,
                "disk_cache_mib": 16,
                "timeout_seconds": 30,
                "checkpoint_duration_seconds": 1,
                "object_latency_ms": 2 if backend == "fixture" else 0,
                "object_latency_injected": backend == "fixture",
            },
            "pos3ql_object_store": {
                "implementation": backend,
                "backing": "test storage",
                "request_metrics_available": request_metrics,
            },
        }
        (directory / "environment.json").write_text(json.dumps(environment))
        metrics = None
        if request_metrics:
            metrics = {
                "requests": {"delete": 1, "get": 2, "list": 3, "put": 4, "range_get": 0}
            }
        labels = {
            "point-concurrency-1": measured(100, 2, metrics),
            "mixed-baseline": measured(80, 3, metrics),
            "mixed-checkpoint-interference": measured(60, 4, metrics),
            "postgresql18-point-concurrency-1": measured(200, 1),
            "postgresql18-mixed-baseline": measured(180, 1.5),
            "postgresql18-mixed-checkpoint-interference": measured(160, 2),
            "postgresql18-matched-point-concurrency-1": measured(190, 1.1),
            "postgresql18-matched-mixed-baseline": measured(170, 1.7),
            "postgresql18-matched-mixed-checkpoint-interference": measured(150, 2.2),
        }
        for label, result in labels.items():
            artifact = {
                "schema_version": 1,
                "label": label,
                "workload": {"name": "test"},
                "results": result,
            }
            (directory / f"{label}.json").write_text(json.dumps(artifact))
        for filename, profile, limits in (
            (
                "postgresql-server.json",
                "host-available",
                {"cpu_quota": 0, "memory_bytes": 0, "memory_swap_bytes": 0},
            ),
            (
                "postgresql-matched-server.json",
                "resource-matched",
                {"cpu_quota": 2, "memory_bytes": 1048576, "memory_swap_bytes": 1048576},
            ),
        ):
            (directory / filename).write_text(
                json.dumps(
                    {
                        "container_image_id": "sha256:postgresql",
                        "resource_profile": profile,
                        "resource_limits": limits,
                    }
                )
            )

    def test_reports_paired_backends_and_unavailable_provider_metrics(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            for backend in ("fixture", "minio", "seaweedfs"):
                self.write_run(root, backend)
            completed = subprocess.run(
                ["python3", str(SCRIPT), str(root)],
                check=True,
                text=True,
                capture_output=True,
            )
            self.assertIn("| fixture | pos3ql | mixed baseline", completed.stdout)
            self.assertIn(
                "| minio | PostgreSQL 18 host available | mixed with checkpoints",
                completed.stdout,
            )
            self.assertIn(
                "| minio | PostgreSQL 18 resource matched | mixed with checkpoints",
                completed.stdout,
            )
            self.assertIn("| seaweedfs | pos3ql | point", completed.stdout)
            self.assertIn("| fixture | fixture | test storage | 2.00 ms | yes |", completed.stdout)
            self.assertIn("| minio | minio | test storage | none | no |", completed.stdout)
            self.assertIn("| 1.000 | 0 |", completed.stdout)
            self.assertIn(
                "| fixture | 2 | 1.00 | 2.00 | 1.00 | 1.00 |",
                completed.stdout,
            )

    def test_rejects_a_different_binary(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            self.write_run(root, "fixture")
            self.write_run(root, "minio", binary="other")
            self.write_run(root, "seaweedfs")
            completed = subprocess.run(
                ["python3", str(SCRIPT), str(root)],
                check=False,
                text=True,
                capture_output=True,
            )
            self.assertNotEqual(completed.returncode, 0)
            self.assertIn("minio used a different pos3ql binary", completed.stderr)


if __name__ == "__main__":
    unittest.main()

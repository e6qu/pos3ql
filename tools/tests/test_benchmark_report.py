import json
import pathlib
import subprocess
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
TOOL = ROOT / "tools" / "benchmark-report.py"


class BenchmarkReportTests(unittest.TestCase):
    def test_labels_independently_operated_service_and_recorded_conditions(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = pathlib.Path(temporary)
            environment = {
                "schema_version": 1,
                "artifact_type": "environment",
                "git_commit": "0123456789abcdef",
                "pos3ql_binary_sha256": "abcdef0123456789abcdef0123456789",
                "machine": "test-machine",
                "logical_cpu_count": 8,
                "hardware_description": "pinned test host",
                "suite": {
                    "mode": "full",
                    "rows": 100,
                    "clients": 4,
                    "disk_cache_mib": 1024,
                    "cache_storage_description": "local NVMe",
                    "timeout_seconds": 120,
                    "object_latency_injected": False,
                    "object_latency_ms": 0,
                },
                "pos3ql_object_store": {
                    "implementation": "qualified service version 1",
                    "backing": "provider-managed durable storage",
                    "container_image": None,
                    "container_image_id": None,
                    "independently_operated": True,
                    "request_metrics_available": False,
                    "endpoint": "objects.example:443",
                    "bucket": "performance",
                    "prefix": "run-1",
                    "region": "region-1",
                    "addressing": "virtual_hosted",
                    "tls": True,
                    "tls_ca_sha256": "58d2cc8bcded4f950c7ff22643544f2e8a92cf6c454858caf0e52b316054303f",
                    "network_description": "same-region private network",
                },
            }
            workload = {
                "schema_version": 1,
                "artifact_type": "benchmark",
                "label": "point-concurrency-1",
                "database_identity": "PostgreSQL 18.4 (pos3ql 0.1.0)",
                "workload": "point-read",
                "results": {
                    "throughput_ops_per_second": 10,
                    "latency_ms": {
                        "p50": 1,
                        "p95": 2,
                        "p99": 3,
                        "maximum": 4,
                    },
                    "completed_operations": 20,
                    "errors": [],
                },
            }
            (directory / "environment.json").write_text(json.dumps(environment))
            (directory / "point.json").write_text(json.dumps(workload))
            completed = subprocess.run(
                [sys.executable, str(TOOL), str(directory)],
                cwd=ROOT,
                check=True,
                text=True,
                capture_output=True,
            )

        self.assertIn(
            "Benchmark host: pinned test host; cache storage: local NVMe.",
            completed.stdout,
        )
        self.assertIn("independently operated", completed.stdout)
        self.assertIn("endpoint `objects.example:443`", completed.stdout)
        self.assertIn("TLS CA `58d2cc8bcded4f950c7f…`", completed.stdout)
        self.assertIn("network: same-region private network", completed.stdout)
        self.assertNotIn("local-host timing is exploratory", completed.stdout)


if __name__ == "__main__":
    unittest.main()

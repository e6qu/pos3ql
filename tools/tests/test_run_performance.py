import os
import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
TOOL = ROOT / "tools" / "run-performance.sh"


class RunPerformanceTests(unittest.TestCase):
    def external_environment(self):
        environment = os.environ.copy()
        environment.update(
            {
                "POS3QL_BENCH_OBJECT_STORE": "external",
                "POS3QL_BENCH_OBJECT_STORE_ENDPOINT": "objects.example:443",
                "POS3QL_BENCH_OBJECT_STORE_BUCKET": "performance",
                "POS3QL_BENCH_OBJECT_STORE_REGION": "region-1",
                "POS3QL_BENCH_OBJECT_STORE_ACCESS_KEY": "test-access",
                "POS3QL_BENCH_OBJECT_STORE_SECRET_KEY": "do-not-print",
                "POS3QL_BENCH_OBJECT_STORE_IMPLEMENTATION": "test service version 1",
                "POS3QL_BENCH_OBJECT_STORE_BACKING": "test durable storage",
                "POS3QL_BENCH_HARDWARE_DESCRIPTION": "test host",
                "POS3QL_BENCH_NETWORK_DESCRIPTION": "test network",
                "POS3QL_BENCH_CACHE_STORAGE": "test cache",
            }
        )
        return environment

    def run_harness(self, environment):
        with tempfile.TemporaryDirectory() as temporary:
            return subprocess.run(
                [str(TOOL), "smoke", str(pathlib.Path(temporary) / "output")],
                cwd=ROOT,
                env=environment,
                text=True,
                capture_output=True,
            )

    def test_external_service_requires_operation_assertion_without_echoing_secrets(self):
        completed = self.run_harness(self.external_environment())

        self.assertEqual(completed.returncode, 2)
        self.assertIn("INDEPENDENTLY_OPERATED=1", completed.stderr)
        self.assertNotIn("do-not-print", completed.stderr)

    def test_external_service_rejects_plaintext(self):
        environment = self.external_environment()
        environment["POS3QL_BENCH_OBJECT_STORE_INDEPENDENTLY_OPERATED"] = "1"
        environment["POS3QL_BENCH_OBJECT_STORE_TLS"] = "off"
        completed = self.run_harness(environment)

        self.assertEqual(completed.returncode, 2)
        self.assertIn("require POS3QL_BENCH_OBJECT_STORE_TLS=on", completed.stderr)
        self.assertNotIn("do-not-print", completed.stderr)


if __name__ == "__main__":
    unittest.main()

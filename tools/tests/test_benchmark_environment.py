import json
import pathlib
import subprocess
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
TOOL = ROOT / "tools" / "benchmark-environment.py"


class BenchmarkEnvironmentTests(unittest.TestCase):
    def arguments(self, directory, independently_operated="1", tls="1", prefix="run-1"):
        binary = directory / "pos3ql"
        binary.write_bytes(b"benchmark binary")
        tls_ca = directory / "ca.pem"
        tls_ca.write_bytes(b"test trust root")
        return [
            sys.executable,
            str(TOOL),
            "--output",
            str(directory / "environment.json"),
            "--binary",
            str(binary),
            "--mode",
            "full",
            "--rows",
            "100",
            "--table-capacity",
            "512",
            "--operations",
            "20",
            "--clients",
            "4",
            "--replicas",
            "2",
            "--object-latency-ms",
            "0",
            "--object-latency-injected",
            "0",
            "--object-store-implementation",
            "qualified service version 1",
            "--object-store-backing",
            "provider-managed durable storage",
            "--object-store-independent-implementation",
            "1",
            "--object-store-independently-operated",
            independently_operated,
            "--object-store-request-metrics",
            "0",
            "--object-store-endpoint",
            "objects.example:443",
            "--object-store-bucket",
            "performance",
            "--object-store-prefix",
            prefix,
            "--object-store-region",
            "region-1",
            "--object-store-addressing",
            "virtual_hosted",
            "--object-store-tls",
            tls,
            "--object-store-tls-ca-file",
            str(tls_ca),
            "--hardware-description",
            "fixed host type",
            "--network-description",
            "same-region private network",
            "--cache-storage-description",
            "local NVMe",
            "--disk-cache-mib",
            "1024",
            "--timeout-seconds",
            "120",
        ]

    def test_records_external_service_and_host_provenance(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = pathlib.Path(temporary)
            subprocess.run(self.arguments(directory), cwd=ROOT, check=True)
            result = json.loads((directory / "environment.json").read_text())

        self.assertEqual(result["hardware_description"], "fixed host type")
        self.assertEqual(result["suite"]["cache_storage_description"], "local NVMe")
        store = result["pos3ql_object_store"]
        self.assertTrue(store["independently_operated"])
        self.assertTrue(store["tls"])
        self.assertEqual(store["endpoint"], "objects.example:443")
        self.assertEqual(store["prefix"], "run-1")
        self.assertEqual(
            store["tls_ca_sha256"],
            "58d2cc8bcded4f950c7ff22643544f2e8a92cf6c454858caf0e52b316054303f",
        )
        self.assertEqual(store["network_description"], "same-region private network")

    def test_rejects_plaintext_independently_operated_service(self):
        with tempfile.TemporaryDirectory() as temporary:
            completed = subprocess.run(
                self.arguments(pathlib.Path(temporary), tls="0"),
                cwd=ROOT,
                text=True,
                capture_output=True,
            )

        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("must use TLS", completed.stderr)

    def test_rejects_shared_prefix_for_independently_operated_service(self):
        with tempfile.TemporaryDirectory() as temporary:
            completed = subprocess.run(
                self.arguments(pathlib.Path(temporary), prefix=""),
                cwd=ROOT,
                text=True,
                capture_output=True,
            )

        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("requires an isolated prefix", completed.stderr)


if __name__ == "__main__":
    unittest.main()

import importlib.util
import pathlib
import sys
import types
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools"))
SPEC = importlib.util.spec_from_file_location(
    "benchmark_postgresql", ROOT / "tools" / "benchmark-postgresql.py"
)
postgresql = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(postgresql)


class BenchmarkPostgresqlTest(unittest.TestCase):
    def setUp(self):
        self.original_connection = postgresql.PgConnection
        self.original_inspect = postgresql.docker_inspect

        class Connection:
            def __init__(self, *_args):
                pass

            def close(self):
                pass

            def query(self, sql):
                if "server_version_num" in sql:
                    return [["180006", "PostgreSQL 18.6 test"]]
                return [
                    [name, "on" if name in {
                        "fsync", "full_page_writes", "synchronous_commit"
                    } else "1", "", "default"]
                    for name in postgresql.SETTINGS
                ]

        postgresql.PgConnection = Connection
        postgresql.docker_inspect = lambda _container, template: {
            "{{.Image}}": "sha256:image",
            "{{json .Mounts}}": "[]",
            "{{.HostConfig.NanoCpus}}": "2000000000",
            "{{.HostConfig.Memory}}": "1048576",
            "{{.HostConfig.MemorySwap}}": "1048576",
        }[template]

    def tearDown(self):
        postgresql.PgConnection = self.original_connection
        postgresql.docker_inspect = self.original_inspect

    def args(self, memory=1048576):
        return types.SimpleNamespace(
            host="127.0.0.1",
            port=5432,
            user="postgres",
            database="postgres",
            storage_description="test storage",
            docker_container="postgres",
            resource_profile="resource-matched",
            expected_cpus=2.0,
            expected_memory_bytes=memory,
        )

    def test_records_and_validates_docker_resource_limits(self):
        result = postgresql.capture(self.args())
        self.assertEqual(result["resource_profile"], "resource-matched")
        self.assertEqual(
            result["resource_limits"],
            {"cpu_quota": 2.0, "memory_bytes": 1048576, "memory_swap_bytes": 1048576},
        )

    def test_rejects_a_mismatched_memory_limit(self):
        with self.assertRaisesRegex(ValueError, "memory limit"):
            postgresql.capture(self.args(memory=2097152))

    def test_rejects_limits_for_host_available_profile(self):
        args = self.args()
        args.resource_profile = "host-available"
        args.expected_cpus = None
        args.expected_memory_bytes = None
        with self.assertRaisesRegex(ValueError, "has container resource limits"):
            postgresql.capture(args)


if __name__ == "__main__":
    unittest.main()

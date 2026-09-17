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

    def test_brin_point_uses_the_dedicated_summary_key(self):
        self.assertEqual(
            benchmark.workload_sql("brin-point", 7, 19, 1000),
            "SELECT payload FROM benchmark_kv WHERE brin_key = 331",
        )
        self.assertEqual(
            benchmark.workload_sql("brin-inclusion", 7, 19, 1000),
            "SELECT payload FROM benchmark_kv WHERE brin_span && '[662,663)'::int4range",
        )
        self.assertEqual(
            benchmark.workload_sql("gist-inclusion", 7, 19, 1000),
            "SELECT payload FROM benchmark_kv WHERE gist_span && '[993,994)'::int4range",
        )
        self.assertEqual(
            benchmark.workload_sql("gist-multirange", 7, 19, 1000),
            "SELECT payload FROM benchmark_kv "
            "WHERE gist_spans && '{[2317,2318)}'::int4multirange",
        )
        self.assertEqual(
            benchmark.workload_sql("gist-network", 7, 19, 1000),
            "SELECT payload FROM benchmark_kv "
            "WHERE gist_address <<= '10.0.1.75'",
        )
        self.assertEqual(
            benchmark.workload_sql("gist-knn", 7, 19, 1000),
            "SELECT id, payload FROM benchmark_kv "
            "ORDER BY gist_location <-> point '(331,331)' LIMIT 8",
        )
        self.assertEqual(
            benchmark.workload_sql("gin-array", 7, 19, 1000),
            "SELECT payload FROM benchmark_kv WHERE gin_tags @> ARRAY[331]",
        )
        self.assertEqual(
            benchmark.workload_sql("gin-array-overlap", 7, 19, 1000),
            "SELECT payload FROM benchmark_kv WHERE gin_tags && ARRAY[331,1331]",
        )
        self.assertEqual(
            benchmark.workload_sql("gin-tsvector", 7, 19, 1000),
            "SELECT payload FROM benchmark_kv WHERE gin_document @@ 'token331'::tsquery",
        )
        self.assertEqual(
            benchmark.workload_sql("gist-tsvector", 7, 19, 1000),
            "SELECT payload FROM benchmark_kv WHERE gist_document @@ 'gisttoken331'::tsquery",
        )
        self.assertEqual(
            benchmark.workload_sql("gin-jsonb", 7, 19, 1000),
            "SELECT payload FROM benchmark_kv WHERE json_ops ? 'key331'",
        )
        self.assertEqual(
            benchmark.workload_sql("gin-jsonb-path", 7, 19, 1000),
            "SELECT payload FROM benchmark_kv WHERE json_path @> '{\"token\":\"value331\"}'::jsonb",
        )
        self.assertEqual(
            benchmark.workload_sql("spgist-prefix", 7, 19, 1000),
            "SELECT payload FROM benchmark_kv WHERE spgist_label ^@ 'key-331'",
        )
        self.assertEqual(
            benchmark.workload_sql("spgist-range", 7, 19, 1000),
            "SELECT payload FROM benchmark_kv WHERE spgist_span && '[1655,1656)'::int4range",
        )
        self.assertEqual(
            benchmark.workload_sql("spgist-network", 7, 19, 1000),
            "SELECT payload FROM benchmark_kv "
            "WHERE spgist_address <<= '11.0.1.75'",
        )
        self.assertEqual(
            benchmark.workload_sql("spgist-knn", 7, 19, 1000),
            "SELECT id, payload FROM benchmark_kv "
            "ORDER BY spgist_location <-> point '(331,-331)' LIMIT 8",
        )
        self.assertEqual(
            benchmark.workload_sql("tail-range", 0, 0, 8),
            "SELECT sum(payload), count(*) FROM benchmark_kv WHERE id >= 1",
        )

    def test_ordered_limit_is_a_covering_bounded_result(self):
        self.assertEqual(
            benchmark.workload_sql("ordered-limit", 7, 19, 1000),
            "SELECT id, payload FROM benchmark_kv ORDER BY id DESC LIMIT 32",
        )

    def test_join_probe_uses_a_bounded_parameterized_key_set(self):
        self.assertEqual(
            benchmark.workload_sql("join-probe", 7, 19, 1000),
            "SELECT sum(kv.payload), count(*) "
            "FROM generate_series(331, 362) AS probe(id) "
            "JOIN benchmark_kv AS kv ON kv.id = probe.id",
        )
        self.assertEqual(
            benchmark.workload_sql("join-probe", 0, 99, 100),
            "SELECT sum(kv.payload), count(*) "
            "FROM generate_series(69, 100) AS probe(id) "
            "JOIN benchmark_kv AS kv ON kv.id = probe.id",
        )

    def test_spatial_workloads_probe_existing_gist_and_spgist_generations(self):
        self.assertEqual(
            benchmark.workload_sql("gist-spatial", 7, 19, 1000),
            "SELECT id, payload FROM benchmark_kv WHERE gist_location <@ box '(331,331),(331,331)'",
        )
        self.assertEqual(
            benchmark.workload_sql("spgist-spatial", 7, 19, 1000),
            "SELECT id, payload FROM benchmark_kv WHERE spgist_location <@ box '(331,-331),(331,-331)'",
        )

    def test_insert_workload_leaves_the_fixed_row_body_at_its_default(self):
        self.assertEqual(
            benchmark.workload_sql("insert", 2, 7, 1000),
            "INSERT INTO benchmark_kv(id, hash_key, brin_key, brin_span, gist_span, gist_spans, gist_address, gist_location, gin_tags, gin_document, gist_document, json_ops, json_path, spgist_label, spgist_span, spgist_address, spgist_location, payload) VALUES (2001008, 2001008, 2001008, '[4002016,4002018)'::int4range, '[6003024,6003027)'::int4range, int4multirange(int4range(14007056,14007058), int4range(14007060,14007062)), '10.0.0.0'::inet + 2001008, point(2001008, 2001008), ARRAY[2001008], to_tsvector('simple','token2001008'), to_tsvector('simple','gisttoken2001008'), jsonb_build_object('key2001008','value2001008'), jsonb_build_object('token','value2001008'), 'key-2001008', int4range(10005040,10005042), '11.0.0.0'::inet + 2001008, point(2001008, -2001008), 0)",
        )

    def test_point_reads_and_updates_exercise_the_hash_key(self):
        self.assertEqual(
            benchmark.workload_sql("point-read", 7, 19, 1000),
            "SELECT payload FROM benchmark_kv WHERE hash_key = 331",
        )
        self.assertEqual(
            benchmark.workload_sql("update", 7, 19, 1000),
            "UPDATE benchmark_kv SET payload = payload + 1 WHERE hash_key = 331",
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

    def test_ordered_limit_validation_requires_index_only_execution(self):
        result = {
            "workload": {"name": "ordered-limit", "require_index": True},
            "results": {
                "attempted_operations": 2,
                "completed_operations": 2,
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
                    "index_scans": 2,
                    "index_tuples_fetched": 1,
                    "sequential_scans": 0,
                    "sequential_tuples_read": 0,
                },
            },
        }
        self.assertIn("covering ordered workload fetched base tuples", benchmark.validate(result))

        for workload in ("gist-knn", "spgist-knn"):
            result["workload"]["name"] = workload
            self.assertIn(
                "covering ordered workload fetched base tuples",
                benchmark.validate(result),
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

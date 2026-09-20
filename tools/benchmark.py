#!/usr/bin/env python3
"""Dependency-free PostgreSQL-wire benchmark and machine-readable result writer."""

import argparse
import json
import math
import os
import pathlib
import socket
import statistics
import struct
import subprocess
import threading
import time


RESULT_SCHEMA_VERSION = 1


def frontend(kind, payload=b""):
    return kind + struct.pack("!i", len(payload) + 4) + payload


def recv_exact(connection, length):
    result = bytearray()
    while len(result) < length:
        chunk = connection.recv(length - len(result))
        if not chunk:
            raise ConnectionError("unexpected PostgreSQL connection EOF")
        result.extend(chunk)
    return bytes(result)


def read_message(connection):
    header = recv_exact(connection, 5)
    length = struct.unpack("!i", header[1:])[0]
    if length < 4:
        raise ConnectionError(f"invalid PostgreSQL message length {length}")
    return header[:1], recv_exact(connection, length - 4)


def error_fields(payload):
    fields = {}
    for field in payload.split(b"\0"):
        if field:
            fields[field[:1].decode(errors="replace")] = field[1:].decode(errors="replace")
    return fields


class PgConnection:
    def __init__(self, host, port, user, database):
        self.socket = socket.create_connection((host, port), timeout=30)
        self.socket.settimeout(30)
        parameters = (
            b"user\0" + user.encode() + b"\0"
            + b"database\0" + database.encode() + b"\0"
            + b"application_name\0pos3ql-benchmark\0\0"
        )
        startup = struct.pack("!i", 196608) + parameters
        self.socket.sendall(struct.pack("!i", len(startup) + 4) + startup)
        self._drain(expect_rows=False)

    def close(self):
        try:
            self.socket.sendall(frontend(b"X"))
        except OSError:
            pass
        self.socket.close()

    def send_query(self, sql):
        self.socket.sendall(frontend(b"Q", sql.encode() + b"\0"))

    def receive_query(self):
        return self._drain(expect_rows=True)

    def query(self, sql):
        self.send_query(sql)
        return self.receive_query()

    def _drain(self, expect_rows):
        rows = []
        error = None
        while True:
            kind, payload = read_message(self.socket)
            if kind == b"R" and struct.unpack("!i", payload[:4])[0] != 0:
                raise RuntimeError("benchmark requires trust authentication")
            if kind == b"E":
                error = error_fields(payload)
            elif kind == b"D" and expect_rows:
                count = struct.unpack("!h", payload[:2])[0]
                at = 2
                row = []
                for _ in range(count):
                    length = struct.unpack("!i", payload[at : at + 4])[0]
                    at += 4
                    if length == -1:
                        row.append(None)
                    else:
                        row.append(payload[at : at + length].decode(errors="replace"))
                        at += length
                rows.append(row)
            elif kind == b"Z":
                if error:
                    raise RuntimeError(
                        f"PostgreSQL error {error.get('C', '?????')}: "
                        f"{error.get('M', 'unknown error')}"
                    )
                return rows


def read_metrics(path):
    if not path:
        return None
    metrics_path = pathlib.Path(path)
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        try:
            return json.loads(metrics_path.read_text(encoding="utf-8"))
        except (FileNotFoundError, json.JSONDecodeError):
            time.sleep(0.05)
    raise RuntimeError(f"object-store metrics did not appear at {path}")


def subtract_metrics(after, before):
    if after is None or before is None:
        return None
    return {
        "schema_version": after["schema_version"],
        "requests": {
            operation: after["requests"][operation] - before["requests"][operation]
            for operation in sorted(after["requests"])
        },
        "request_body_bytes": after["request_body_bytes"] - before["request_body_bytes"],
        "response_body_bytes": after["response_body_bytes"] - before["response_body_bytes"],
        "errors": after["errors"] - before["errors"],
    }


def process_rss_bytes(pid):
    if not pid:
        return None
    status = pathlib.Path(f"/proc/{pid}/status")
    if status.exists():
        for line in status.read_text(encoding="ascii").splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) * 1024
    try:
        return int(subprocess.check_output(
            ["ps", "-o", "rss=", "-p", str(pid)], text=True
        ).strip()) * 1024
    except (OSError, ValueError, subprocess.CalledProcessError):
        return None


def process_cpu_seconds(pid):
    if not pid:
        return None
    stat = pathlib.Path(f"/proc/{pid}/stat")
    if not stat.exists():
        return None
    fields = stat.read_text(encoding="ascii").split()
    ticks = os.sysconf("SC_CLK_TCK")
    return (int(fields[13]) + int(fields[14])) / ticks


class RssSampler:
    def __init__(self, pid):
        self.pid = pid
        self.maximum = process_rss_bytes(pid)
        self.stop_event = threading.Event()
        self.thread = None

    def start(self):
        if not self.pid:
            return
        self.thread = threading.Thread(target=self._sample, daemon=True)
        self.thread.start()

    def _sample(self):
        while not self.stop_event.wait(0.01):
            value = process_rss_bytes(self.pid)
            if value is not None:
                self.maximum = value if self.maximum is None else max(self.maximum, value)

    def stop(self):
        self.stop_event.set()
        if self.thread:
            self.thread.join()


def percentile(sorted_values, percent):
    if not sorted_values:
        return None
    index = max(0, math.ceil(percent / 100 * len(sorted_values)) - 1)
    return sorted_values[index] / 1_000_000


def ipv4_offset(first_octet, offset):
    value = (first_octet << 24) + offset
    return ".".join(str((value >> shift) & 0xFF) for shift in (24, 16, 8, 0))


def workload_sql(workload, worker, operation, rows):
    key = (worker + operation * 17) % rows + 1
    if workload == "point-read" or (workload == "mixed" and operation % 5):
        return f"SELECT payload FROM benchmark_kv WHERE hash_key = {key}"
    if workload == "brin-point":
        return f"SELECT payload FROM benchmark_kv WHERE brin_key = {key}"
    if workload == "brin-inclusion":
        lower = key * 2
        return (
            "SELECT payload FROM benchmark_kv "
            f"WHERE brin_span && '[{lower},{lower + 1})'::int4range"
        )
    if workload == "gist-inclusion":
        lower = key * 3
        return (
            "SELECT payload FROM benchmark_kv "
            f"WHERE gist_span && '[{lower},{lower + 1})'::int4range"
        )
    if workload == "gist-multirange":
        lower = key * 7
        return (
            "SELECT payload FROM benchmark_kv "
            f"WHERE gist_spans && '{{[{lower},{lower + 1})}}'::int4multirange"
        )
    if workload == "gist-network":
        return (
            "SELECT payload FROM benchmark_kv "
            f"WHERE gist_address <<= '{ipv4_offset(10, key)}'"
        )
    if workload == "gist-spatial":
        return (
            "SELECT id, payload FROM benchmark_kv "
            f"WHERE gist_location <@ box '({key},{key}),({key},{key})'"
        )
    if workload == "gist-knn":
        return (
            "SELECT id, payload FROM benchmark_kv "
            f"ORDER BY gist_location <-> point '({key},{key})' LIMIT 8"
        )
    if workload == "gin-array":
        return f"SELECT payload FROM benchmark_kv WHERE gin_tags @> ARRAY[{key}]"
    if workload == "gin-array-overlap":
        return (
            "SELECT payload FROM benchmark_kv "
            f"WHERE gin_tags && ARRAY[{key},{rows + key}]"
        )
    if workload == "gin-tsvector":
        return (
            "SELECT payload FROM benchmark_kv "
            f"WHERE gin_document @@ 'token{key}'::tsquery"
        )
    if workload == "gist-tsvector":
        return (
            "SELECT payload FROM benchmark_kv "
            f"WHERE gist_document @@ 'gisttoken{key}'::tsquery"
        )
    if workload == "gin-jsonb":
        return f"SELECT payload FROM benchmark_kv WHERE json_ops ? 'key{key}'"
    if workload == "gin-jsonb-path":
        return (
            "SELECT payload FROM benchmark_kv "
            f"WHERE json_path @> '{{\"token\":\"value{key}\"}}'::jsonb"
        )
    if workload == "spgist-prefix":
        return f"SELECT payload FROM benchmark_kv WHERE spgist_label ^@ 'key-{key}'"
    if workload == "spgist-range":
        lower = key * 5
        return (
            "SELECT payload FROM benchmark_kv "
            f"WHERE spgist_span && '[{lower},{lower + 1})'::int4range"
        )
    if workload == "spgist-network":
        return (
            "SELECT payload FROM benchmark_kv "
            f"WHERE spgist_address <<= '{ipv4_offset(11, key)}'"
        )
    if workload == "spgist-spatial":
        return (
            "SELECT id, payload FROM benchmark_kv "
            f"WHERE spgist_location <@ box '({key},{-key}),({key},{-key})'"
        )
    if workload == "spgist-knn":
        return (
            "SELECT id, payload FROM benchmark_kv "
            f"ORDER BY spgist_location <-> point '({key},{-key})' LIMIT 8"
        )
    if workload in ("update", "mixed"):
        return f"UPDATE benchmark_kv SET payload = payload + 1 WHERE hash_key = {key}"
    if workload == "scan":
        return "SELECT sum(payload), count(*) FROM benchmark_kv"
    if workload == "tail-range":
        lower = max(1, rows - 31)
        return (
            "SELECT sum(payload), count(*) FROM benchmark_kv "
            f"WHERE id >= {lower}"
        )
    if workload == "ordered-limit":
        return "SELECT id, payload FROM benchmark_kv ORDER BY id DESC LIMIT 32"
    if workload == "join-probe":
        lower = max(1, min(key, rows - 31))
        upper = min(rows, lower + 31)
        return (
            "SELECT sum(kv.payload), count(*) "
            f"FROM generate_series({lower}, {upper}) AS probe(id) "
            "JOIN benchmark_kv AS kv ON kv.id = probe.id"
        )
    if workload == "insert":
        inserted_key = rows + worker * 1_000_000 + operation + 1
        return (
            "INSERT INTO benchmark_kv(id, hash_key, brin_key, brin_span, gist_span, gist_spans, gist_address, gist_location, gin_tags, gin_document, gist_document, json_ops, json_path, spgist_label, spgist_span, spgist_address, spgist_location, payload) "
            f"VALUES ({inserted_key}, {inserted_key}, {inserted_key}, "
            f"'[{inserted_key * 2},{inserted_key * 2 + 2})'::int4range, "
            f"'[{inserted_key * 3},{inserted_key * 3 + 3})'::int4range, "
            f"int4multirange(int4range({inserted_key * 7},{inserted_key * 7 + 2}), int4range({inserted_key * 7 + 4},{inserted_key * 7 + 6})), "
            f"'10.0.0.0'::inet + {inserted_key}, point({inserted_key}, {inserted_key}), "
            f"ARRAY[{inserted_key}], to_tsvector('simple','token{inserted_key}'), "
            f"to_tsvector('simple','gisttoken{inserted_key}'), jsonb_build_object('key{inserted_key}','value{inserted_key}'), "
            f"jsonb_build_object('token','value{inserted_key}'), 'key-{inserted_key}', int4range({inserted_key * 5},{inserted_key * 5 + 2}), "
            f"'11.0.0.0'::inet + {inserted_key}, "
            f"point({inserted_key}, {-inserted_key}), 0)"
        )
    raise ValueError(f"unknown workload {workload}")


def identify(connection):
    rows = connection.query("SELECT version()")
    return rows[0][0] if rows and rows[0] else "unknown"


def setup_database(connection, rows):
    # Setup is untimed and may build many indexes over a large fixture.
    connection.socket.settimeout(300)
    connection.query("DROP TABLE IF EXISTS benchmark_kv")
    # A realistic row body makes even the small CI dataset span immutable
    # table blocks, so a selective cold index probe competes against an actual
    # multi-block table scan instead of a degenerate one-block fixture.
    connection.query(
        "CREATE TABLE benchmark_kv("
        "id integer PRIMARY KEY, hash_key integer NOT NULL, brin_key integer NOT NULL, "
        "brin_span int4range NOT NULL, gist_span int4range NOT NULL, "
        "gist_spans int4multirange NOT NULL, gist_address inet NOT NULL, "
        "gist_location point NOT NULL, "
        "gin_tags integer[] NOT NULL, gin_document tsvector NOT NULL, "
        "gist_document tsvector NOT NULL, json_ops jsonb NOT NULL, json_path jsonb NOT NULL, "
        "spgist_label text NOT NULL, spgist_span int4range NOT NULL, "
        "spgist_address inet NOT NULL, spgist_location point NOT NULL, "
        "payload bigint NOT NULL, "
        "padding text NOT NULL DEFAULT repeat('x', 8192))"
    )
    # Keep setup valid for startup-sized transaction pools smaller than the
    # dataset; each chunk is its own implicit transaction on both engines.
    for first in range(1, rows + 1, 100):
        last = min(rows, first + 99)
        connection.query(
            "INSERT INTO benchmark_kv(id, hash_key, brin_key, brin_span, gist_span, gist_spans, gist_address, gist_location, gin_tags, gin_document, gist_document, json_ops, json_path, spgist_label, spgist_span, spgist_address, spgist_location, payload) "
            "SELECT value, value, value, int4range(value * 2, value * 2 + 2), "
            "int4range(value * 3, value * 3 + 3), "
            "int4multirange(int4range(value * 7, value * 7 + 2), int4range(value * 7 + 4, value * 7 + 6)), "
            "'10.0.0.0'::inet + value, point(value, value), ARRAY[value], "
            "to_tsvector('simple','token' || value::text), "
            "to_tsvector('simple','gisttoken' || value::text), "
            "jsonb_build_object('key' || value::text,'value' || value::text), "
            "jsonb_build_object('token','value' || value::text), "
            "'key-' || value::text, int4range(value * 5, value * 5 + 2), "
            "'11.0.0.0'::inet + value, point(value, -value), 0 "
            f"FROM generate_series({first}, {last}) AS value"
        )
    # Build secondary indexes after the bulk load; measured writes still maintain them.
    connection.query(
        "CREATE INDEX benchmark_hash_lookup ON benchmark_kv USING hash (hash_key)"
    )
    connection.query(
        "CREATE INDEX benchmark_brin_lookup ON benchmark_kv USING brin (brin_key) "
        "WITH (pages_per_range=32, autosummarize=on)"
    )
    connection.query(
        "CREATE INDEX benchmark_brin_inclusion ON benchmark_kv USING brin "
        "(brin_span range_inclusion_ops) WITH (pages_per_range=32)"
    )
    connection.query(
        "CREATE INDEX benchmark_gist_inclusion ON benchmark_kv USING gist (gist_span)"
    )
    connection.query(
        "CREATE INDEX benchmark_gist_multirange ON benchmark_kv USING gist (gist_spans)"
    )
    connection.query(
        "CREATE INDEX benchmark_gist_network ON benchmark_kv USING gist (gist_address inet_ops)"
    )
    connection.query(
        "CREATE INDEX benchmark_gist_knn ON benchmark_kv USING gist (gist_location) INCLUDE (id, payload)"
    )
    connection.query(
        "CREATE INDEX benchmark_gin_array ON benchmark_kv USING gin (gin_tags)"
    )
    connection.query(
        "CREATE INDEX benchmark_gin_document ON benchmark_kv USING gin (gin_document)"
    )
    connection.query(
        "CREATE INDEX benchmark_gist_document ON benchmark_kv USING gist (gist_document)"
    )
    connection.query(
        "CREATE INDEX benchmark_gin_json_ops ON benchmark_kv USING gin (json_ops)"
    )
    connection.query(
        "CREATE INDEX benchmark_gin_json_path ON benchmark_kv USING gin (json_path jsonb_path_ops)"
    )
    connection.query(
        "CREATE INDEX benchmark_spgist_prefix ON benchmark_kv USING spgist (spgist_label)"
    )
    connection.query(
        "CREATE INDEX benchmark_spgist_range ON benchmark_kv USING spgist (spgist_span)"
    )
    connection.query(
        "CREATE INDEX benchmark_spgist_network ON benchmark_kv USING spgist (spgist_address inet_ops)"
    )
    connection.query(
        "CREATE INDEX benchmark_spgist_knn ON benchmark_kv USING spgist (spgist_location kd_point_ops) INCLUDE (id, payload)"
    )
    connection.query(
        "CREATE INDEX benchmark_covering "
        "ON benchmark_kv (id DESC) INCLUDE (payload)"
    )
    connection.query("ANALYZE benchmark_kv")
    connection.query("CHECKPOINT")
    connection.socket.settimeout(30)


def read_access_path(connection):
    rows = connection.query(
        "SELECT seq_scan, seq_tup_read, idx_scan, idx_tup_fetch "
        "FROM pg_stat_user_tables "
        "WHERE schemaname = 'public' AND relname = 'benchmark_kv'"
    )
    if len(rows) != 1 or len(rows[0]) != 4:
        return None
    names = ("sequential_scans", "sequential_tuples_read", "index_scans", "index_tuples_fetched")
    try:
        return {name: int(value or 0) for name, value in zip(names, rows[0])}
    except ValueError as error:
        raise RuntimeError("invalid pg_stat_user_tables counters") from error


def subtract_access_path(after, before):
    if after is None or before is None:
        return None
    return {name: after[name] - before[name] for name in sorted(after)}


def run(args):
    targets = args.targets or [(args.host, args.port)]
    control = PgConnection(*targets[0], args.user, args.database)
    try:
        identity = identify(control)
        if args.setup:
            setup_database(control, args.rows)
    finally:
        control.close()

    target_identities = []
    for host, port in targets:
        connection = PgConnection(host, port, args.user, args.database)
        try:
            target_identities.append(identify(connection))
        finally:
            connection.close()
    connections = [PgConnection(*targets[index % len(targets)], args.user, args.database)
                   for index in range(args.clients)]
    latencies = []
    errors = []
    before_access_path = None
    if len(targets) == 1:
        try:
            before_access_path = read_access_path(connections[0])
        except Exception as error:
            errors.append(f"access-path baseline: {error}")
    result_lock = threading.Lock()
    start_barrier = threading.Barrier(args.clients + 1)
    operation_barrier = threading.Barrier(args.clients) if args.synchronized else None
    stop_maintenance = threading.Event()
    maintenance_count = [0]

    def worker(worker_id):
        local = []
        try:
            start_barrier.wait()
            for operation in range(args.operations):
                if operation_barrier:
                    operation_barrier.wait()
                sql = workload_sql(args.workload, worker_id, operation, args.rows)
                started = time.perf_counter_ns()
                connections[worker_id].query(sql)
                local.append(time.perf_counter_ns() - started)
        except Exception as error:  # surfaced in the result and by the exit code
            with result_lock:
                errors.append(f"worker {worker_id}: {error}")
            # One failed synchronized worker can never reach the next
            # generation. Wake every peer with BrokenBarrierError instead of
            # leaving the benchmark hung forever and losing the first error.
            if operation_barrier:
                operation_barrier.abort()
        finally:
            with result_lock:
                latencies.extend(local)

    def maintain():
        connection = PgConnection(*targets[0], args.user, args.database)
        try:
            while not stop_maintenance.wait(args.maintenance_interval):
                connection.query("CHECKPOINT")
                maintenance_count[0] += 1
                if args.maintenance_limit and maintenance_count[0] >= args.maintenance_limit:
                    break
        except Exception as error:
            with result_lock:
                errors.append(f"maintenance: {error}")
        finally:
            connection.close()

    if args.object_metrics:
        time.sleep(0.1)
    before_metrics = read_metrics(args.object_metrics)
    before_cpu = process_cpu_seconds(args.pid)
    sampler = RssSampler(args.pid)
    sampler.start()
    maintenance = None
    if args.maintenance_interval:
        maintenance = threading.Thread(target=maintain)
        maintenance.start()
    workers = [threading.Thread(target=worker, args=(index,)) for index in range(args.clients)]
    for thread in workers:
        thread.start()
    started = time.perf_counter_ns()
    start_barrier.wait()
    for thread in workers:
        thread.join()
    elapsed_seconds = (time.perf_counter_ns() - started) / 1_000_000_000
    stop_maintenance.set()
    if maintenance:
        maintenance.join()
    sampler.stop()
    after_cpu = process_cpu_seconds(args.pid)
    # The sidecar publishes snapshots every 50 ms.
    if args.object_metrics:
        time.sleep(0.1)
    after_metrics = read_metrics(args.object_metrics)
    for connection in connections:
        connection.close()
    after_access_path = None
    if len(targets) == 1:
        try:
            # PostgreSQL flushes backend statistics when the workers exit.
            statistics_connection = PgConnection(*targets[0], args.user, args.database)
            try:
                after_access_path = read_access_path(statistics_connection)
            finally:
                statistics_connection.close()
        except Exception as error:
            errors.append(f"access-path result: {error}")

    ordered = sorted(latencies)
    completed = len(ordered)
    object_metrics = subtract_metrics(after_metrics, before_metrics)
    object_requests = (
        sum(object_metrics["requests"].values()) if object_metrics is not None else None
    )
    result = {
        "schema_version": RESULT_SCHEMA_VERSION,
        "label": args.label,
        "database_identity": identity,
        "target_identities": target_identities,
        "workload": {
            "name": args.workload,
            "clients": args.clients,
            "operations_per_client": args.operations,
            "rows": args.rows,
            "synchronized": args.synchronized,
            "require_index": args.require_index,
            "maintenance_interval_seconds": args.maintenance_interval,
            "maintenance_limit": args.maintenance_limit,
            "target_count": len(targets),
        },
        "results": {
            "attempted_operations": args.clients * args.operations,
            "completed_operations": completed,
            "errors": errors,
            "elapsed_seconds": elapsed_seconds,
            "throughput_ops_per_second": completed / elapsed_seconds if elapsed_seconds else None,
            "latency_ms": {
                "minimum": ordered[0] / 1_000_000 if ordered else None,
                "p50": percentile(ordered, 50),
                "p95": percentile(ordered, 95),
                "p99": percentile(ordered, 99),
                "maximum": ordered[-1] / 1_000_000 if ordered else None,
                "mean": statistics.fmean(ordered) / 1_000_000 if ordered else None,
            },
            "process_cpu_seconds": (
                after_cpu - before_cpu
                if after_cpu is not None and before_cpu is not None
                else None
            ),
            "maximum_rss_bytes": sampler.maximum,
            "fixed_memory_plan_bytes": args.fixed_memory_bytes,
            "fixed_memory_occupancy": (
                sampler.maximum / args.fixed_memory_bytes
                if sampler.maximum is not None and args.fixed_memory_bytes
                else None
            ),
            "maintenance_operations": maintenance_count[0],
            "access_path": subtract_access_path(after_access_path, before_access_path),
            "object_store": object_metrics,
            "object_requests_per_operation": (
                object_requests / completed
                if object_requests is not None and completed
                else None
            ),
        },
    }
    return result


def validate(result):
    failures = []
    results = result["results"]
    if results["errors"]:
        failures.append("workload reported errors")
    if results["completed_operations"] != results["attempted_operations"]:
        failures.append("not every attempted operation completed")
    latency = results["latency_ms"]
    ordered = [latency[name] for name in ("minimum", "p50", "p95", "p99", "maximum")]
    if any(value is None for value in ordered) or ordered != sorted(ordered):
        failures.append("latency percentiles are absent or unordered")
    occupancy = results["fixed_memory_occupancy"]
    if occupancy is not None and occupancy > 1.25:
        failures.append("RSS exceeded the fixed memory plan by more than 25%")
    metrics = results["object_store"]
    if metrics is not None and set(metrics["requests"]) != {
        "delete", "get", "list", "put", "range_get"
    }:
        failures.append("object-store instrumentation has the wrong operation schema")
    workload = result.get("workload", {})
    access_path = results.get("access_path")
    if workload.get("require_index"):
        if access_path is None:
            failures.append("required index-access counters are unavailable")
        else:
            if access_path["index_scans"] < results["completed_operations"]:
                failures.append("workload did not execute an index scan per operation")
            if access_path["sequential_scans"] != 0:
                failures.append("workload unexpectedly executed sequential scans")
            if (
                workload.get("name") in ("ordered-limit", "gist-knn", "spgist-knn")
                and access_path["index_tuples_fetched"] != 0
            ):
                failures.append("covering ordered workload fetched base tuples")
    if (
        metrics is not None
        and workload.get("name") == "update"
        and workload.get("synchronized")
        and workload.get("clients", 1) > 1
        and metrics["requests"]["put"] > results["attempted_operations"] * 1.75
    ):
        failures.append("concurrent commit PUT amplification exceeded the regression bound")
    return failures


def parse_args():
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int)
    parser.add_argument(
        "--target",
        action="append",
        default=[],
        help="repeatable host:port target; connections are distributed round-robin",
    )
    parser.add_argument("--user", default="postgres")
    parser.add_argument("--database", default="postgres")
    parser.add_argument("--label", required=True)
    parser.add_argument(
        "--workload",
        choices=(
            "point-read",
            "brin-point",
            "brin-inclusion",
            "gist-inclusion",
            "gist-multirange",
            "gist-network",
            "gist-spatial",
            "gist-knn",
            "gin-array",
            "gin-array-overlap",
            "gin-tsvector",
            "gist-tsvector",
            "gin-jsonb",
            "gin-jsonb-path",
            "spgist-prefix",
            "spgist-range",
            "spgist-network",
            "spgist-spatial",
            "spgist-knn",
            "tail-range",
            "ordered-limit",
            "join-probe",
            "update",
            "mixed",
            "scan",
            "insert",
        ),
        required=True,
    )
    parser.add_argument("--clients", type=int, default=1)
    parser.add_argument("--operations", type=int, default=100)
    parser.add_argument("--rows", type=int, default=1000)
    parser.add_argument("--setup", action="store_true")
    parser.add_argument("--synchronized", action="store_true")
    parser.add_argument(
        "--require-index",
        action="store_true",
        help="fail validation unless every operation uses an index and none uses a sequential scan",
    )
    parser.add_argument("--maintenance-interval", type=float, default=0.0)
    parser.add_argument("--maintenance-limit", type=int, default=0)
    parser.add_argument("--object-metrics")
    parser.add_argument("--pid", type=int)
    parser.add_argument("--fixed-memory-bytes", type=int)
    parser.add_argument("--output")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    if not args.port and not args.target:
        parser.error("--port or at least one --target is required")
    args.targets = []
    for target in args.target:
        try:
            host, raw_port = target.rsplit(":", 1)
            args.targets.append((host, int(raw_port)))
        except (ValueError, TypeError):
            parser.error(f"invalid target {target!r}; expected host:port")
    if args.clients < 1 or args.operations < 1 or args.rows < 1:
        parser.error("clients, operations, and rows must be positive")
    if args.maintenance_interval < 0:
        parser.error("maintenance interval cannot be negative")
    if args.maintenance_limit < 0 or (args.maintenance_limit and not args.maintenance_interval):
        parser.error("maintenance limit requires a positive interval")
    if args.require_index and (
        args.workload
        not in (
            "point-read",
            "brin-point",
            "brin-inclusion",
            "gist-inclusion",
            "gist-multirange",
            "gist-network",
            "gist-spatial",
            "gist-knn",
            "gin-array",
            "gin-array-overlap",
            "gin-tsvector",
            "gist-tsvector",
            "gin-jsonb",
            "gin-jsonb-path",
            "spgist-prefix",
            "spgist-range",
            "spgist-network",
            "spgist-spatial",
            "spgist-knn",
            "tail-range",
            "ordered-limit",
            "join-probe",
            "update",
        )
        or len(args.targets) > 1
    ):
        parser.error(
            "--require-index requires a point-read, BRIN, GiST, GIN, SP-GiST, tail-range, ordered-limit, join-probe, or update workload against one target"
        )
    return args


def main():
    args = parse_args()
    result = run(args)
    rendered = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.output:
        pathlib.Path(args.output).write_text(rendered, encoding="utf-8")
    print(rendered, end="")
    if args.check:
        failures = validate(result)
        if failures:
            raise SystemExit("benchmark validation failed: " + "; ".join(failures))


if __name__ == "__main__":
    main()

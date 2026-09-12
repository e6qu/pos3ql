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


def workload_sql(workload, worker, operation, rows):
    key = (worker + operation * 17) % rows + 1
    if workload == "point-read" or (workload == "mixed" and operation % 5):
        return f"SELECT payload FROM benchmark_kv WHERE id = {key}"
    if workload in ("update", "mixed"):
        return f"UPDATE benchmark_kv SET payload = payload + 1 WHERE id = {key}"
    if workload == "scan":
        return "SELECT sum(payload), count(*) FROM benchmark_kv"
    if workload == "insert":
        inserted_key = rows + worker * 1_000_000 + operation + 1
        return f"INSERT INTO benchmark_kv VALUES ({inserted_key}, 0)"
    raise ValueError(f"unknown workload {workload}")


def identify(connection):
    rows = connection.query("SELECT version()")
    return rows[0][0] if rows and rows[0] else "unknown"


def setup_database(connection, rows):
    connection.query("DROP TABLE IF EXISTS benchmark_kv")
    connection.query("CREATE TABLE benchmark_kv(id integer PRIMARY KEY, payload bigint NOT NULL)")
    # Keep setup valid for startup-sized transaction pools smaller than the
    # dataset; each chunk is its own implicit transaction on both engines.
    for first in range(1, rows + 1, 1000):
        last = min(rows, first + 999)
        connection.query(
            "INSERT INTO benchmark_kv "
            f"SELECT value, 0 FROM generate_series({first}, {last}) AS value"
        )
    connection.query("CHECKPOINT")


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
        finally:
            with result_lock:
                latencies.extend(local)

    def maintain():
        connection = PgConnection(*targets[0], args.user, args.database)
        try:
            while not stop_maintenance.wait(args.maintenance_interval):
                connection.query("CHECKPOINT")
                maintenance_count[0] += 1
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
            "maintenance_interval_seconds": args.maintenance_interval,
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
        choices=("point-read", "update", "mixed", "scan", "insert"),
        required=True,
    )
    parser.add_argument("--clients", type=int, default=1)
    parser.add_argument("--operations", type=int, default=100)
    parser.add_argument("--rows", type=int, default=1000)
    parser.add_argument("--setup", action="store_true")
    parser.add_argument("--synchronized", action="store_true")
    parser.add_argument("--maintenance-interval", type=float, default=0.0)
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

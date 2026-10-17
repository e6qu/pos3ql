#!/usr/bin/env python3
"""Compare writes during an owned object GET with PostgreSQL and cold recovery."""

import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack
import hashlib
import http.server
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import threading
import time

import psycopg

from s3_test_server import S3TestServer
from test_ports import update_reservations


class DownloadGate:
    def __init__(self):
        self.lock = threading.Lock()
        self.started = threading.Event()
        self.release = threading.Event()
        self.failure_sent = threading.Event()
        self.armed_key = None
        self.missing = False
        self.key = None

    def arm(self, key, missing):
        with self.lock:
            self.armed_key, self.missing, self.key = key, missing, key
            self.started.clear()
            self.release.clear()
            self.failure_sent.clear()

    def disarm(self):
        with self.lock:
            self.armed_key = None

    def wait(self, command, key):
        with self.lock:
            claimed = command == "GET" and key == self.armed_key
            missing = self.missing
        if not claimed:
            return
        self.started.set()
        if not self.release.wait(10):
            raise TimeoutError("test did not release its held block response")
        if missing:
            self.failure_sent.set()
            raise FileNotFoundError("injected missing block response")


class PausingS3Server(S3TestServer):
    gate = None

    def begin(self):
        body, parsed, key = super().begin()
        self.gate.wait(self.command, key)
        return body, parsed, key


def result(connection, statement):
    cursor = connection.execute(statement)
    columns = [] if cursor.description is None else [
        (column.name, column.type_code) for column in cursor.description
    ]
    rows = [] if cursor.description is None else cursor.fetchall()
    return {"columns": columns, "rows": rows, "tag": cursor.statusmessage}


def setup(connection, schema):
    statements = [
        f"DROP SCHEMA IF EXISTS {schema} CASCADE",
        f"CREATE SCHEMA {schema}",
        f"CREATE TABLE {schema}.warm_orders(id integer PRIMARY KEY, note text)",
        f"CREATE TABLE {schema}.cold_invoices(id integer PRIMARY KEY, note text)",
        f"INSERT INTO {schema}.warm_orders VALUES (1, 'warm one'), (2, 'warm two'), (3, 'warm three')",
        f"INSERT INTO {schema}.cold_invoices VALUES (10, 'cold ten'), (11, 'cold eleven'), (12, 'cold twelve')",
        f"CREATE SEQUENCE {schema}.write_calls",
        f"""CREATE FUNCTION {schema}.count_write() RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN
                NEW.note := NEW.note || ':' || nextval('{schema}.write_calls')::text;
                RETURN NEW;
            END $$""",
        f"CREATE TRIGGER count_write BEFORE INSERT OR UPDATE ON {schema}.warm_orders "
        f"FOR EACH ROW EXECUTE FUNCTION {schema}.count_write()",
        f"""CREATE FUNCTION {schema}.count_constraint() RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN
                PERFORM nextval('{schema}.write_calls');
                IF NEW.id < 0 THEN
                    RAISE EXCEPTION 'negative order' USING ERRCODE = '23514';
                END IF;
                RETURN NEW;
            END $$""",
        f"CREATE CONSTRAINT TRIGGER count_constraint AFTER INSERT OR UPDATE ON {schema}.warm_orders "
        f"DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION {schema}.count_constraint()",
    ]
    for statement in statements:
        connection.execute(statement)


def current_state(connection, schema):
    return {
        "orders": result(connection, f"SELECT id, note FROM {schema}.warm_orders ORDER BY id"),
        "sequence": result(connection, f"SELECT last_value FROM {schema}.write_calls"),
    }


def exercise(connection, schema):
    observed = []
    for statement in [
        f"UPDATE {schema}.warm_orders SET note = 'paid' WHERE id = 1 RETURNING id, note",
        f"INSERT INTO {schema}.warm_orders VALUES (4, 'new') RETURNING id, note",
        f"DELETE FROM {schema}.warm_orders WHERE id = 2 RETURNING id, note",
    ]:
        observed.append(result(connection, statement))
    with connection.cursor() as cursor:
        with cursor.copy(f"COPY {schema}.warm_orders(id, note) FROM STDIN") as copy:
            copy.write_row((5, "copied"))
        observed.append({"copy_tag": cursor.statusmessage})
    for copy_input in [False, True]:
        try:
            if copy_input:
                with connection.cursor() as cursor:
                    with cursor.copy(f"COPY {schema}.warm_orders(id, note) FROM STDIN") as copy:
                        copy.write_row((-2, "rejected copy"))
            else:
                connection.execute(f"INSERT INTO {schema}.warm_orders VALUES (-1, 'rejected insert')")
        except psycopg.Error as error:
            assert error.sqlstate == "23514", error
            observed.append({"copy_input": copy_input, "error": error.sqlstate})
        else:
            raise AssertionError("deferred trigger failure was acknowledged as a commit")
    observed.append(result(connection, "BEGIN"))
    observed.append(result(connection,
        f"UPDATE {schema}.warm_orders SET note = 'rollback' WHERE id = 1 RETURNING id, note"))
    observed.append(result(connection, "SET CONSTRAINTS ALL IMMEDIATE"))
    observed.append(result(connection, "ROLLBACK"))
    return {"operations": observed, "state": current_state(connection, schema)}


class Node:
    def __init__(self, binary, directory, port, endpoint, prefix):
        self.binary = binary
        self.directory, self.port = directory, port
        self.data = directory / "data"
        self.config = directory / "server.conf"
        self.log_path = directory / "server.log"
        self.process = None
        directory.mkdir(parents=True)
        values = {
            "listen_addr": f"127.0.0.1:{port}", "data_dir": self.data,
            "max_connections": 4, "query_workspace_slots": 2,
            "memtable_bytes": "4MiB", "wal_bytes": "4MiB", "wal_buffer_bytes": "256KiB",
            "sql_arena_bytes": "1MiB", "work_arena_bytes": "8MiB",
            "object_store": "on", "object_store_endpoint": endpoint,
            "object_store_bucket": "atomic-write-test", "object_store_prefix": prefix,
            "object_store_region": "test-region", "object_store_access_key": "test-access",
            "object_store_secret_key": "test-secret", "object_store_get_slots": 2,
            "object_store_response_bytes": "256KiB", "wal_upload": "on", "wal_upload_sync": "on",
            "block_cache_bytes": "4MiB", "disk_cache_bytes": 0,
            "max_tables": 8, "table_rows": 128, "txn_rows": 256,
            "max_ddl_per_transaction": 16, "max_value_indexes": 8, "value_index_rows": 256,
            "max_views": 4, "max_materialized_views": 4, "max_routines": 8,
            "max_domains": 4, "max_enums": 4, "max_composites": 4, "max_triggers": 8,
            "max_prepared_transactions": 2, "max_subscriptions": 1,
        }
        descriptor = os.open(self.config, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "w") as output:
            output.write("".join(f"{name} = {value}\n" for name, value in values.items()))

    def connect(self):
        return psycopg.connect(host="127.0.0.1", port=self.port, user="postgres",
            dbname="postgres", autocommit=True, connect_timeout=1, prepare_threshold=None)

    def start(self):
        with self.log_path.open("ab") as log:
            self.process = subprocess.Popen([str(self.binary), "--config", str(self.config)],
                stdout=log, stderr=log)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError(f"test server exited at startup: {self.log_path}")
            try:
                with self.connect() as connection:
                    assert connection.execute("SELECT 1").fetchone() == (1,)
                return
            except psycopg.OperationalError:
                time.sleep(0.05)
        raise TimeoutError(f"test server did not accept SQL: {self.log_path}")

    def stop(self, crash=False):
        if self.process is None:
            return
        process, self.process = self.process, None
        if process.poll() is not None:
            if not crash:
                raise RuntimeError(f"test server exited unexpectedly: {process.returncode}")
            return
        process.send_signal(signal.SIGKILL if crash else signal.SIGTERM)
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
            raise TimeoutError("owned test server did not finish shutdown")
        if not crash and process.returncode != 0:
            raise RuntimeError(f"test server shutdown failed: {process.returncode}")

    def wipe_cache(self):
        if self.data.is_symlink() or self.data.resolve().parent != self.directory.resolve():
            raise RuntimeError("refusing to remove a path outside this test server")
        shutil.rmtree(self.data)


def scenario(reference, node, gate, object_root, prefix, missing, schema):
    report = (f"SELECT c.id, c.note, (SELECT note FROM {schema}.warm_orders WHERE id = 1) "
        f"FROM {schema}.cold_invoices AS c ORDER BY c.id")
    setup(reference, schema)
    expected_report = result(reference, report)
    expected_write = exercise(reference, schema)
    node.start()
    with node.connect() as connection:
        setup(connection, schema)
        connection.execute("CHECKPOINT")
    node.stop()
    blocks = object_root / "atomic-write-test" / prefix / "blocks"
    candidates = [path for path in blocks.iterdir()
        if path.is_file() and b"cold ten" in path.read_bytes()]
    assert len(candidates) == 1, "fixture needs one object containing its required cold invoice value"
    target = candidates[0]
    target_bytes = target.read_bytes()
    assert b"warm one" not in target_bytes, "invoice fault must not target the cached order"
    target_key = target.relative_to(object_root / "atomic-write-test").as_posix()
    node.wipe_cache()
    node.start()
    with node.connect() as writer, node.connect() as reader:
        writer.execute(f"SELECT id, note FROM {schema}.warm_orders ORDER BY id").fetchall()
        writer.execute(f"SELECT note FROM {schema}.warm_orders WHERE id = 1").fetchall()
        head = object_root / "atomic-write-test" / prefix / "commit-head"
        before = head.read_bytes()
        # Lookahead can request a block that the final plan never consumes.
        # Hold every request for the object containing a required result value.
        gate.arm(target_key, missing)
        with ThreadPoolExecutor(max_workers=2) as workers:
            pending = workers.submit(result, reader, report)
            try:
                assert gate.started.wait(5), "cold report did not start an object GET"
                write = workers.submit(exercise, writer, schema)
                actual_write = write.result(timeout=5)
                assert actual_write == expected_write, (actual_write, expected_write)
                assert not pending.done(), "cold report finished before the writer acknowledgement"
                assert not gate.release.is_set(), "writer progress accidentally released the held GET"
                after = head.read_bytes()
                assert after != before, "writer acknowledged without advancing remote commit-head"
            finally:
                gate.release.set()
            if missing:
                try:
                    pending.result(timeout=5)
                except psycopg.Error as error:
                    assert error.sqlstate == "58030", error
                    assert gate.failure_sent.is_set(), "error did not come from the injected GET"
                else:
                    raise AssertionError("failed block response was hidden")
            else:
                actual_report = pending.result(timeout=5)
                assert actual_report == expected_report, (actual_report, expected_report)
            gate.disarm()
            assert current_state(writer, schema) == expected_write["state"]
    node.stop(crash=True)
    node.wipe_cache()
    node.start()
    with node.connect() as connection:
        recovered = current_state(connection, schema)
        assert recovered == expected_write["state"], (recovered, expected_write["state"])
    node.stop()
    print(json.dumps({"case": "missing" if missing else "success", "held_key": gate.key,
        "postgresql_matches": True, "snapshot_preserved": not missing,
        "durable_ack_before_release": True, "cold_recovery": True,
        "commit_head_sha256": hashlib.sha256(after).hexdigest(),
        "held_object_sha256": hashlib.sha256(target_bytes).hexdigest(),
        "state": expected_write["state"]}, sort_keys=True), flush=True)


def stop_http(server, thread):
    try:
        server.shutdown()
    finally:
        thread.join(timeout=5)
        if thread.is_alive():
            raise TimeoutError("owned HTTP fixture did not stop")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pg", required=True, type=int)
    parser.add_argument("--host", required=True)
    parser.add_argument("--user", required=True)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--work", required=True, type=Path)
    args = parser.parse_args()
    args.binary = args.binary.resolve(strict=True)
    args.work.mkdir(parents=True)
    state = Path(os.environ.get("POS3QL_TEST_PORT_STATE",
        f"/tmp/pos3ql-test-ports-{os.getuid()}.json"))
    owner = os.getpid()
    gate = DownloadGate()
    node = None
    with ExitStack() as resources:
        port = update_reservations(state, owner, first=16000, last=16099)
        resources.callback(update_reservations, state, owner)
        object_root = args.work / "object-store"
        (object_root / "atomic-write-test").mkdir(parents=True)
        PausingS3Server.root = object_root.resolve()
        PausingS3Server.bucket = "atomic-write-test"
        PausingS3Server.region = "test-region"
        PausingS3Server.access_key = "test-access"
        PausingS3Server.secret_key = "test-secret"
        PausingS3Server.gate = gate
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), PausingS3Server)
        server.daemon_threads = True
        resources.callback(server.server_close)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        resources.callback(stop_http, server, thread)
        resources.callback(gate.release.set)
        try:
            with psycopg.connect(host=args.host, port=args.pg, user=args.user, dbname="postgres",
                    autocommit=True, prepare_threshold=None) as reference:
                version = int(reference.execute("SHOW server_version_num").fetchone()[0])
                assert 180000 <= version < 190000, f"PostgreSQL 18 is required, got {version}"
                print(json.dumps({"reference_version": version,
                    "binary_sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest()}), flush=True)
                for missing in [False, True]:
                    case = "missing" if missing else "success"
                    prefix = f"atomic-{case}/"
                    schema = f"atomic_io_{owner}_{case}"
                    node = Node(args.binary, args.work / case, port,
                        f"127.0.0.1:{server.server_port}", prefix)
                    with ExitStack() as case_resources:
                        case_resources.callback(reference.execute, f"DROP SCHEMA IF EXISTS {schema} CASCADE")
                        case_resources.callback(node.stop)
                        case_resources.callback(gate.disarm)
                        case_resources.callback(gate.release.set)
                        scenario(reference, node, gate, object_root, prefix, missing, schema)
            print("ok: atomic writes match PostgreSQL during held GETs; both cold recoveries passed", flush=True)
        except BaseException:
            if node is not None and node.log_path.exists():
                print("owned server log (last 60 lines):", flush=True)
                print("\n".join(node.log_path.read_text(errors="replace").splitlines()[-60:]), flush=True)
            raise


if __name__ == "__main__":
    main()

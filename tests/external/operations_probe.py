#!/usr/bin/env python3
"""Black-box checks for the bounded operational HTTP listener."""

import json
import os
import pathlib
import select
import struct
import socket
import time


PORT = int(os.environ["POS3QL_OPERATIONS_PORT"])
DATABASE_PORT = int(os.environ["POS3QL_PORT"])
POSTGRES_CONNECTIONS = int(os.environ.get("POS3QL_EXPECTED_CONNECTIONS", "8"))
QUERY_WORKSPACES = int(os.environ.get("POS3QL_EXPECTED_QUERY_WORKSPACES", "1"))
OBJECT_STORE = os.environ.get("POS3QL_EXPECTED_OBJECT_STORE", "on") == "on"


def read_response(stream):
    chunks = []
    while True:
        chunk = stream.recv(65536)
        if not chunk:
            break
        chunks.append(chunk)
    head, body = b"".join(chunks).split(b"\r\n\r\n", 1)
    status = int(head.split(b" ", 2)[1])
    headers = {}
    for line in head.split(b"\r\n")[1:]:
        name, value = line.split(b":", 1)
        headers[name.lower()] = value.strip()
    assert int(headers[b"content-length"]) == len(body)
    assert headers[b"cache-control"] == b"no-store"
    return status, body


def request(method: str, path: str, fragmented: bool = False):
    wire = f"{method} {path} HTTP/1.1\r\nHost: localhost\r\n\r\n".encode()
    with socket.create_connection(("127.0.0.1", PORT), timeout=5) as stream:
        if fragmented:
            stream.sendall(wire[:7])
            stream.sendall(wire[7:])
        else:
            stream.sendall(wire)
        return read_response(stream)


def cached_sql_while_probe_waits():
    with socket.create_connection(("127.0.0.1", DATABASE_PORT), timeout=2) as stream:
        stream.settimeout(2)
        source = stream.makefile("rb")

        def frame():
            head = source.read(5)
            assert len(head) == 5, "SQL connection closed during a delayed health check"
            length = struct.unpack("!I", head[1:])[0]
            assert length >= 4
            body = source.read(length - 4)
            assert len(body) == length - 4
            assert head[:1] != b"E", body
            return head[:1], body

        startup = struct.pack("!I", 3 << 16) + b"user\0postgres\0database\0postgres\0\0"
        stream.sendall(struct.pack("!I", len(startup) + 4) + startup)
        while frame()[0] != b"Z":
            pass
        query = b"SELECT 42\0"
        stream.sendall(b"Q" + struct.pack("!I", len(query) + 4) + query)
        rows = []
        while True:
            kind, body = frame()
            if kind == b"D":
                rows.append(body)
            if kind == b"Z":
                break
        assert rows == [b"\0\x01\0\0\0\x0242"]
        source.close()


def delayed_writer_probe():
    gate = pathlib.Path(os.environ["POS3QL_WRITER_PROBE_GATE_FILE"])
    observed = gate.with_suffix(".observed")
    assert not gate.exists() and not observed.exists(), "probe gate must start clean"
    with socket.create_connection(("127.0.0.1", PORT), timeout=5) as held, \
            socket.create_connection(("127.0.0.1", PORT), timeout=5) as metrics:
        gate.write_text("hold", encoding="ascii")
        try:
            try:
                held.sendall(b"GET /readyz HTTP/1.1\r\nHost: localhost\r\n\r\n")
                held.shutdown(socket.SHUT_WR)
                deadline = time.monotonic() + 2
                while not observed.exists():
                    assert time.monotonic() < deadline, "readiness did not start its writer probe"
                    time.sleep(0.005)
                metrics.sendall(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
                assert not select.select([held], [], [], 0)[0], "pending probe reported readiness early"
                cached_sql_while_probe_waits()
                assert request("GET", "/livez")[0] == 200
                assert request("GET", "/capacity")[0] == 200
                assert observed.read_text(encoding="ascii") == "1", "waiting checks must share the probe"
            finally:
                gate.unlink(missing_ok=True)
            status, body = read_response(held)
            assert status == 200 and json.loads(body) == {"status": "ready"}
            status, body = read_response(metrics)
            assert status == 200 and "pos3ql_ready 1" in body.decode()
            assert request("GET", "/readyz")[0] == 200
        finally:
            gate.unlink(missing_ok=True)
            observed.unlink(missing_ok=True)


status, body = request("GET", "/livez", fragmented=True)
assert status == 200 and json.loads(body) == {"status": "live"}

status, body = request("GET", "/readyz")
assert status == 200 and json.loads(body) == {"status": "ready"}

status, body = request("GET", "/metrics")
metrics = body.decode()
assert status == 200
for sample in [
    "pos3ql_up 1",
    "pos3ql_ready 1",
    f"pos3ql_postgres_connection_capacity {POSTGRES_CONNECTIONS}",
    f"pos3ql_query_workspace_capacity {QUERY_WORKSPACES}",
    "pos3ql_query_workspaces_active 0",
    "pos3ql_query_workspace_waiters 0",
    "pos3ql_foreign_session_capacity",
    "pos3ql_foreign_sessions_used 0",
    "pos3ql_wal_capacity_bytes",
    "pos3ql_row_heap_capacity_bytes",
    "pos3ql_block_object_gets_total",
]:
    assert sample in metrics, sample

status, body = request("GET", "/capacity")
capacity = json.loads(body)
assert status == 200
assert capacity["postgres_connections"]["limit"] == POSTGRES_CONNECTIONS
assert capacity["query_workspace_slots"]["limit"] == QUERY_WORKSPACES
assert capacity["query_workspace_slots"]["used"] == 0
assert capacity["query_workspace_slots"]["waiting"] == 0
assert capacity["foreign_sessions"]["used"] == 0
assert capacity["foreign_sessions"]["limit"] > 0
assert capacity["operational_connections"]["limit"] == 4
assert capacity["object_store"] is OBJECT_STORE
assert capacity["memory"]["core_budget_bytes"] > 0
assert capacity["memory"]["tls_budget_bytes"] > 0

assert request("GET", "/missing")[0] == 404
assert request("POST", "/readyz")[0] == 405

if OBJECT_STORE:
    delayed_writer_probe()

# Operational slots remain usable when every PostgreSQL slot is occupied by a
# client that has connected but not sent its startup packet.
held = [
    socket.create_connection(("127.0.0.1", DATABASE_PORT), timeout=5)
    for _ in range(POSTGRES_CONNECTIONS)
]
try:
    time.sleep(0.1)
    status, body = request("GET", "/capacity")
    assert status == 200
    assert json.loads(body)["postgres_connections"]["used"] == POSTGRES_CONNECTIONS
    assert request("GET", "/livez")[0] == 200
finally:
    for stream in held:
        stream.close()

for _ in range(50):
    status, body = request("GET", "/capacity")
    if status == 200 and json.loads(body)["postgres_connections"]["used"] == 0:
        break
    time.sleep(0.02)
else:
    raise AssertionError("PostgreSQL connection slots were not released")

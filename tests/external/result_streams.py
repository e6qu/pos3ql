#!/usr/bin/env python3
"""A client that stops reading a large result must not stall other sessions.

Client A sends a large simple Query, then the same result through an unpaged
extended Execute (Parse/Bind/Execute(0)/Sync), and stops reading with a small
receive buffer. Two results are used: a generated series (pos3ql queues it in
the result spool) and a scan of one table (pos3ql resumes it row by row). Client B, a psycopg session, must keep completing queries
while A is stalled. A then reads everything: exactly one RowDescription per
simple query, every row once and in order, one CommandComplete and one
ReadyForQuery. With --pg, the same protocol transcript summary is compared
with PostgreSQL 18.

PostgreSQL references:
https://www.postgresql.org/docs/18/protocol-flow.html
https://www.postgresql.org/docs/18/protocol-message-formats.html
"""

import argparse
import socket
import struct
import time

import psycopg

ROWS = 20000
TABLE_ROWS = 5000
# Results must exceed what loopback socket buffers can absorb (Linux grows a
# send buffer to 4 MiB), so the server really holds output for a stalled client.
QUERIES = (
    (f"SELECT g, repeat('x', 200) FROM generate_series(1, {ROWS}) g", ROWS),
    ("SELECT id, payload FROM result_stream_rows", TABLE_ROWS),
)
OTHER_QUERIES = 100
STALL_SECONDS = 2.0


def message(kind, payload=b""):
    return kind + struct.pack("!I", len(payload) + 4) + payload


def startup(sock, user, database):
    parameters = b"user\0" + user.encode() + b"\0database\0" + database.encode() + b"\0\0"
    body = struct.pack("!I", 196608) + parameters
    sock.sendall(struct.pack("!I", len(body) + 4) + body)
    buffer = b""
    while True:
        kind, payload, buffer = read_message(sock, buffer)
        if kind == b"R":
            (code,) = struct.unpack("!I", payload[:4])
            assert code == 0, f"result_streams.py needs trust authentication (got {code})"
        elif kind == b"E":
            raise AssertionError(f"startup failed: {payload!r}")
        elif kind == b"Z":
            assert buffer == b""
            return


def read_message(sock, buffer):
    while True:
        if len(buffer) >= 5:
            (length,) = struct.unpack("!I", buffer[1:5])
            if len(buffer) >= length + 1:
                return buffer[:1], buffer[5 : length + 1], buffer[length + 1 :]
        chunk = sock.recv(65536)
        if not chunk:
            raise AssertionError("server closed the stalled connection")
        buffer += chunk


def read_until_ready(sock, ready_count):
    buffer = b""
    messages = []
    while sum(1 for kind, _ in messages if kind == b"Z") < ready_count:
        kind, payload, buffer = read_message(sock, buffer)
        messages.append((kind, payload))
    assert buffer == b"", "no output follows the final ReadyForQuery"
    return messages


def summarize(messages):
    """Protocol shape with consecutive rows collapsed; rows must be 1..n."""
    shape = []
    expected = 1
    for kind, payload in messages:
        if kind == b"D":
            (length,) = struct.unpack("!i", payload[2:6])
            value = int(payload[6 : 6 + length])
            assert value == expected, f"row {value} arrived where {expected} was due"
            expected += 1
            if shape and shape[-1][0] == "D":
                shape[-1] = ("D", shape[-1][1] + 1)
            else:
                shape.append(("D", 1))
            continue
        if expected != 1:
            expected = 1
        if kind == b"C":
            shape.append(("C", payload.rstrip(b"\0").decode()))
        elif kind == b"E":
            fields = dict(
                (part[:1].decode(), part[1:].decode())
                for part in payload.split(b"\0")
                if part
            )
            shape.append(("E", fields.get("C")))
        else:
            shape.append((kind.decode(), None))
    return shape


def stalled_client(host, port, user, database):
    sock = socket.create_connection((host, port), timeout=60)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
    startup(sock, user, database)
    return sock


def other_session_progress(host, port, user, database):
    started = time.monotonic()
    # The stalled statement itself may still be computing its rows on the
    # single SQL worker; the bound is generous for unoptimized builds.
    with psycopg.connect(
        host=host, port=port, user=user, dbname=database, autocommit=True, connect_timeout=60
    ) as connection:
        connection.execute("SET statement_timeout = '30s'")
        connection.execute("DROP TABLE IF EXISTS result_stream_progress")
        connection.execute("CREATE TABLE result_stream_progress(v int)")
        for value in range(OTHER_QUERIES):
            connection.execute("INSERT INTO result_stream_progress VALUES (%s)", (value,))
            assert connection.execute("SELECT 1").fetchone() == (1,)
        (count,) = connection.execute("SELECT count(*) FROM result_stream_progress").fetchone()
        assert count == OTHER_QUERIES
        connection.execute("DROP TABLE result_stream_progress")
    return time.monotonic() - started


def create_table(host, port, user, database):
    with psycopg.connect(
        host=host, port=port, user=user, dbname=database, autocommit=True, connect_timeout=5
    ) as connection:
        connection.execute("DROP TABLE IF EXISTS result_stream_rows")
        connection.execute("CREATE TABLE result_stream_rows(id integer, payload text)")
        for first in range(1, TABLE_ROWS + 1, 200):
            connection.execute(
                "INSERT INTO result_stream_rows "
                "SELECT g, repeat('x', 1000) FROM generate_series(%s::int, %s::int) g",
                (first, first + 199),
            )


def probe(host, port, user, database):
    create_table(host, port, user, database)
    results = {}
    for (query, rows), protocol in [
        (entry, protocol) for entry in QUERIES for protocol in ("simple", "extended")
    ]:
        sock = stalled_client(host, port, user, database)
        if protocol == "simple":
            request = message(b"Q", query.encode() + b"\0") + message(b"Q", b"SELECT 1\0")
            ready_count = 2
        else:
            request = (
                message(b"P", b"\0" + query.encode() + b"\0\0\0")
                + message(b"B", b"\0\0\0\0\0\0\0\0")
                + message(b"E", b"\0\0\0\0\0")
                + message(b"S")
            )
            ready_count = 1
        sock.sendall(request)
        # Stop reading: the server has far more output than the socket holds.
        time.sleep(STALL_SECONDS / 2)
        elapsed = other_session_progress(host, port, user, database)
        # Before output stopped blocking the worker, this session never finished.
        assert elapsed < 60, f"{protocol}: another session needed {elapsed:.1f}s"
        time.sleep(STALL_SECONDS / 2)
        transcript = summarize(read_until_ready(sock, ready_count))
        sock.close()
        results[(query, protocol)] = transcript
        print(f"{protocol} {query!r}: {transcript} (other session {elapsed:.2f}s)", flush=True)
        if protocol == "simple":
            expected = [
                ("T", None),
                ("D", rows),
                ("C", f"SELECT {rows}"),
                ("Z", None),
                ("T", None),
                ("D", 1),
                ("C", "SELECT 1"),
                ("Z", None),
            ]
        else:
            expected = [("1", None), ("2", None), ("D", rows), ("C", f"SELECT {rows}"), ("Z", None)]
        assert transcript == expected, transcript
    with psycopg.connect(
        host=host, port=port, user=user, dbname=database, autocommit=True, connect_timeout=5
    ) as connection:
        connection.execute("DROP TABLE result_stream_rows")
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--p3", type=int, required=True, help="pos3ql port")
    parser.add_argument("--pg", type=int, help="PostgreSQL 18 reference port")
    parser.add_argument("--user", default="postgres")
    parser.add_argument("--database", default="postgres")
    args = parser.parse_args()
    actual = probe(args.host, args.p3, args.user, args.database)
    if args.pg is not None:
        with psycopg.connect(
            host=args.host, port=args.pg, user=args.user, dbname="postgres", connect_timeout=5
        ) as connection:
            version = connection.info.server_version
        assert version // 10000 == 18, f"reference is PostgreSQL {version}"
        reference = probe(args.host, args.pg, args.user, args.database)
        assert actual == reference, (actual, reference)
    print("ok: stalled result clients do not stall other sessions")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""A client that stops reading a large result must not stall other sessions.

Client A sends a large simple Query, then the same result through an unpaged
extended Execute (Parse/Bind/Execute(0)/Sync), and stops reading with a small
receive buffer. Client B, a psycopg session, must keep completing queries
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
QUERY = f"SELECT g, repeat('x', 200) FROM generate_series(1, {ROWS}) g"
OTHER_QUERIES = 200
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
    with psycopg.connect(
        host=host, port=port, user=user, dbname=database, autocommit=True, connect_timeout=5
    ) as connection:
        connection.execute("SET statement_timeout = '5s'")
        connection.execute("CREATE TEMP TABLE progress(v int)")
        for value in range(OTHER_QUERIES):
            connection.execute("INSERT INTO progress VALUES (%s)", (value,))
            assert connection.execute("SELECT 1").fetchone() == (1,)
        (count,) = connection.execute("SELECT count(*) FROM progress").fetchone()
        assert count == OTHER_QUERIES
    return time.monotonic() - started


def probe(host, port, user, database):
    results = {}
    for protocol in ("simple", "extended"):
        sock = stalled_client(host, port, user, database)
        if protocol == "simple":
            request = message(b"Q", QUERY.encode() + b"\0") + message(b"Q", b"SELECT 2\0")
            ready_count = 2
        else:
            request = (
                message(b"P", b"\0" + QUERY.encode() + b"\0\0\0")
                + message(b"B", b"\0\0\0\0\0\0\0\0")
                + message(b"E", b"\0\0\0\0\0")
                + message(b"S")
            )
            ready_count = 1
        sock.sendall(request)
        # Stop reading: the server has far more output than the socket holds.
        time.sleep(STALL_SECONDS / 2)
        elapsed = other_session_progress(host, port, user, database)
        assert elapsed < 30, f"{protocol}: another session needed {elapsed:.1f}s"
        time.sleep(STALL_SECONDS / 2)
        transcript = summarize(read_until_ready(sock, ready_count))
        sock.close()
        results[protocol] = transcript
        print(f"{protocol}: {transcript} (other session {elapsed:.2f}s)", flush=True)
    expected_simple = [
        ("T", None),
        ("D", ROWS),
        ("C", f"SELECT {ROWS}"),
        ("Z", None),
        ("T", None),
        ("D", 1),
        ("C", "SELECT 1"),
        ("Z", None),
    ]
    expected_extended = [
        ("1", None),
        ("2", None),
        ("D", ROWS),
        ("C", f"SELECT {ROWS}"),
        ("Z", None),
    ]
    assert results["simple"] == expected_simple, results["simple"]
    assert results["extended"] == expected_extended, results["extended"]
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

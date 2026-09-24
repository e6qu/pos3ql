#!/usr/bin/env python3
"""Compare wide join execution with PostgreSQL 18 over raw v3 wire."""

import argparse
import socket
import struct
import sys


def recv_exact(stream, length):
    output = bytearray()
    while len(output) < length:
        chunk = stream.recv(length - len(output))
        if not chunk:
            raise ConnectionError("unexpected PostgreSQL protocol EOF")
        output.extend(chunk)
    return bytes(output)


def read_message(stream):
    header = recv_exact(stream, 5)
    length = struct.unpack("!i", header[1:])[0]
    return header[:1], recv_exact(stream, length - 4)


def frontend_message(kind, payload=b""):
    return kind + struct.pack("!i", len(payload) + 4) + payload


def text_data_row(payload):
    count = struct.unpack("!H", payload[:2])[0]
    fields = []
    at = 2
    for _ in range(count):
        length = struct.unpack("!i", payload[at : at + 4])[0]
        at += 4
        if length == -1:
            fields.append(None)
        else:
            fields.append(payload[at : at + length].decode())
            at += length
    return fields


def error_fields(payload):
    fields = {}
    at = 0
    while payload[at] != 0:
        code = chr(payload[at])
        end = payload.index(0, at + 1)
        fields[code] = payload[at + 1 : end].decode()
        at = end + 1
    return fields


class Connection:
    def __init__(self, host, port):
        self.stream = socket.create_connection((host, port), timeout=60)
        self.stream.settimeout(60)
        startup = b"user\x00postgres\x00database\x00postgres\x00\x00"
        self.stream.sendall(struct.pack("!ii", len(startup) + 8, 3 << 16) + startup)
        self.read_until_ready("startup")

    def read_until_ready(self, operation):
        rows = []
        error = None
        while True:
            kind, payload = read_message(self.stream)
            if kind == b"D":
                rows.append(text_data_row(payload))
            elif kind == b"E":
                fields = error_fields(payload)
                error = (fields.get("C"), fields.get("M"))
            elif kind == b"Z":
                if error is not None:
                    return error
                return rows
            if kind == b"E" and operation == "startup":
                raise RuntimeError(f"startup failed: {error!r}")

    def query(self, sql):
        self.stream.sendall(frontend_message(b"Q", sql.encode() + b"\x00"))
        return self.read_until_ready(sql[:80])

    def close(self):
        self.stream.close()


def probe(host, port):
    relation_count = 128
    sources = ",".join(f"jcj t{index}" for index in range(relation_count))
    cross = f"SELECT t0.id+t127.id FROM {sources}"
    using = "SELECT id FROM jcj t0" + "".join(
        f" JOIN jcj t{index} USING (id)" for index in range(1, relation_count)
    )
    connection = Connection(host, port)
    try:
        setup = connection.query(
            "CREATE TABLE jcj(id integer); INSERT INTO jcj VALUES (1); "
            "CREATE TABLE join_capacity_target(id integer, note text); "
            "INSERT INTO join_capacity_target VALUES (1, 'before')"
        )
        if setup != []:
            return ("setup", setup)

        cross_rows = connection.query(cross)
        ordered_rows = connection.query(f"{cross} ORDER BY 1")
        window_rows = connection.query(
            f"SELECT row_number() OVER (), t0.id+t127.id FROM {sources}"
        )
        exists_rows = connection.query(f"SELECT EXISTS ({cross})")
        using_rows = connection.query(using)
        explain_ok = isinstance(connection.query(f"EXPLAIN {cross}"), list)

        view_setup = connection.query(
            "CREATE VIEW join_capacity_view AS "
            f"SELECT t0.id+t127.id AS total FROM {sources}"
        )
        view_rows = connection.query("SELECT total FROM join_capacity_view")
        update_rows = connection.query(
            "UPDATE join_capacity_target SET note='after' FROM "
            f"{sources} WHERE t0.id=t127.id RETURNING join_capacity_target.note"
        )
        return (
            cross_rows,
            ordered_rows,
            window_rows,
            exists_rows,
            using_rows,
            explain_ok,
            view_setup,
            view_rows,
            update_rows,
        )
    finally:
        connection.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--pg", type=int, required=True)
    parser.add_argument("--p3", type=int, required=True)
    parser.add_argument("--host", default="127.0.0.1")
    args = parser.parse_args()

    expected = (
        [["2"]],
        [["2"]],
        [["1", "2"]],
        [["t"]],
        [["1"]],
        True,
        [],
        [["2"]],
        [["after"]],
    )
    postgresql = probe(args.host, args.pg)
    pos3ql = probe(args.host, args.p3)
    if postgresql != expected or pos3ql != postgresql:
        print(f"expected:   {expected!r}")
        print(f"PostgreSQL: {postgresql!r}")
        print(f"pos3ql:     {pos3ql!r}")
        return 1
    print("128-relation joins, USING chain, plan, view, window, and DML match PostgreSQL")
    return 0


if __name__ == "__main__":
    sys.exit(main())

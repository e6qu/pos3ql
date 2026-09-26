#!/usr/bin/env python3
"""Compare PostgreSQL 18 relation and record-definition width boundaries."""

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
        if host.startswith("/"):
            self.stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            self.stream.settimeout(120)
            self.stream.connect(f"{host}/.s.PGSQL.{port}")
        else:
            self.stream = socket.create_connection((host, port), timeout=120)
        self.stream.settimeout(120)
        startup = b"user\x00postgres\x00database\x00postgres\x00\x00"
        self.stream.sendall(struct.pack("!ii", len(startup) + 8, 3 << 16) + startup)
        self.read_until_ready()

    def read_until_ready(self):
        messages = []
        while True:
            kind = recv_exact(self.stream, 1)
            length = struct.unpack("!i", recv_exact(self.stream, 4))[0]
            payload = recv_exact(self.stream, length - 4)
            messages.append((kind, payload))
            if kind == b"Z":
                return messages

    def query(self, sql):
        payload = sql.encode() + b"\x00"
        self.stream.sendall(b"Q" + struct.pack("!i", len(payload) + 4) + payload)
        return self.read_until_ready()

    def close(self):
        self.stream.close()


def error_summary(messages):
    errors = [error_fields(payload) for kind, payload in messages if kind == b"E"]
    return None if not errors else (errors[0].get("C"), errors[0].get("M"))


def rows(messages):
    output = []
    for kind, payload in messages:
        if kind != b"D":
            continue
        count = struct.unpack("!H", payload[:2])[0]
        at = 2
        row = []
        for _ in range(count):
            length = struct.unpack("!i", payload[at : at + 4])[0]
            at += 4
            if length == -1:
                row.append(None)
            else:
                row.append(payload[at : at + length].decode())
                at += length
        output.append(tuple(row))
    return output


def definitions(prefix, count):
    return ",".join(f"{prefix}{index} integer" for index in range(count))


def probe(host, port):
    connection = Connection(host, port)
    try:
        connection.query("DROP VIEW IF EXISTS relation_capacity_view")
        connection.query("DROP TABLE IF EXISTS relation_capacity_wide")
        connection.query("DROP TABLE IF EXISTS relation_capacity_left")
        connection.query("DROP TABLE IF EXISTS relation_capacity_right")
        connection.query("DROP TYPE IF EXISTS relation_capacity_composite")
        connection.query("DROP FUNCTION IF EXISTS relation_capacity_result()")
        connection.query("DROP FUNCTION IF EXISTS relation_capacity_result_too_wide()")

        relation_columns = definitions("c", 1600)
        table_create = error_summary(
            connection.query(f"CREATE TABLE relation_capacity_wide ({relation_columns})")
        )
        connection.query(
            "INSERT INTO relation_capacity_wide(c0,c64,c1599) VALUES (1,65,1600)"
        )
        table_row = rows(
            connection.query("SELECT c0,c64,c1599 FROM relation_capacity_wide")
        )
        view_create = error_summary(
            connection.query(
                "CREATE VIEW relation_capacity_view AS SELECT * FROM relation_capacity_wide"
            )
        )
        view_row = rows(connection.query("SELECT c1599 FROM relation_capacity_view"))

        composite_create = error_summary(
            connection.query(
                "CREATE TYPE relation_capacity_composite AS ("
                + definitions("f", 1600)
                + ")"
            )
        )

        record_row = rows(
            connection.query(
                "SELECT c1599 FROM json_to_record('{\"c1599\":1600}') AS r("
                + relation_columns
                + ")"
            )
        )

        join_columns = definitions("c", 80)
        values = ",".join(str(index) for index in range(80))
        connection.query(f"CREATE TABLE relation_capacity_left ({join_columns})")
        connection.query(f"CREATE TABLE relation_capacity_right ({join_columns})")
        connection.query(f"INSERT INTO relation_capacity_left VALUES ({values})")
        connection.query(f"INSERT INTO relation_capacity_right VALUES ({values})")
        using = ",".join(f"c{index}" for index in range(80))
        join_row = rows(
            connection.query(
                "SELECT count(*) FROM relation_capacity_left "
                f"JOIN relation_capacity_right USING ({using})"
            )
        )

        result_columns = definitions("c", 1664)
        routine_create = error_summary(
            connection.query(
                "CREATE FUNCTION relation_capacity_result() RETURNS TABLE ("
                + result_columns
                + ") LANGUAGE plpgsql AS $$BEGIN RETURN NEXT; END$$"
            )
        )
        routine_row = rows(
            connection.query("SELECT c1663 FROM relation_capacity_result()")
        )
        view_error = error_summary(
            connection.query(
                "CREATE VIEW relation_capacity_view_too_wide AS "
                "SELECT * FROM relation_capacity_result()"
            )
        )

        table_error = error_summary(
            connection.query(
                "CREATE TABLE relation_capacity_too_wide ("
                + definitions("c", 1601)
                + ")"
            )
        )
        composite_error = error_summary(
            connection.query(
                "CREATE TYPE relation_capacity_composite_too_wide AS ("
                + definitions("f", 1601)
                + ")"
            )
        )
        record_error = error_summary(
            connection.query(
                "SELECT * FROM json_to_record('{}') AS r("
                + definitions("c", 1601)
                + ")"
            )
        )
        routine_error = error_summary(
            connection.query(
                "CREATE FUNCTION relation_capacity_result_too_wide() RETURNS TABLE ("
                + definitions("c", 1665)
                + ") LANGUAGE plpgsql AS $$BEGIN RETURN NEXT; END$$;"
                + "SELECT * FROM relation_capacity_result_too_wide()"
            )
        )
        return (
            table_create,
            table_row,
            view_create,
            view_row,
            composite_create,
            record_row,
            join_row,
            routine_create,
            routine_row,
            table_error,
            view_error,
            composite_error,
            record_error,
            routine_error,
        )
    finally:
        connection.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--pg", type=int, required=True)
    parser.add_argument("--p3", type=int, required=True)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--pg-host")
    parser.add_argument("--p3-host")
    args = parser.parse_args()

    expected = (
        None,
        [("1", "65", "1600")],
        None,
        [("1600",)],
        None,
        [("1600",)],
        [("1",)],
        None,
        [(None,)],
        ("54011", "tables can have at most 1600 columns"),
        ("54011", "tables can have at most 1600 columns"),
        ("54011", "tables can have at most 1600 columns"),
        ("54011", "column definition lists can have at most 1600 entries"),
        ("54011", "target lists can have at most 1664 entries"),
    )
    postgresql = probe(args.pg_host or args.host, args.pg)
    pos3ql = probe(args.p3_host or args.host, args.p3)
    if postgresql != expected or pos3ql != postgresql:
        print(f"expected:   {expected!r}")
        print(f"PostgreSQL: {postgresql!r}")
        print(f"pos3ql:     {pos3ql!r}")
        return 1
    print("1,600-column relations and 1,664-column routine results match PostgreSQL")
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Compare PostgreSQL's complete prepared-parameter boundary over raw v3 wire."""

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


def parameter_capacity(host, port):
    """Exercise both unsigned wire counts and SQL PREPARE at PostgreSQL's limit."""
    stream = socket.create_connection((host, port), timeout=30)
    stream.settimeout(30)
    startup = b"user\x00postgres\x00database\x00postgres\x00\x00"
    stream.sendall(struct.pack("!ii", len(startup) + 8, 3 << 16) + startup)
    while True:
        kind, payload = read_message(stream)
        if kind == b"E":
            raise RuntimeError(f"startup failed: {payload!r}")
        if kind == b"Z":
            break

    count = 65535
    parameter_types = struct.pack("!H", count) + struct.pack("!i", 23) * count
    parse = frontend_message(
        b"P", b"wide\x00SELECT $65535::integer\x00" + parameter_types
    )
    describe = frontend_message(b"D", b"Swide\x00")
    values = bytearray(struct.pack("!HH", 0, count))
    values.extend(struct.pack("!i", -1) * (count - 1))
    values.extend(struct.pack("!i", 2) + b"42")
    values.extend(struct.pack("!H", 0))
    bind = frontend_message(b"B", b"\x00wide\x00" + values)
    execute = frontend_message(b"E", b"\x00\x00\x00\x00\x00")
    stream.sendall(parse + describe + bind + execute + frontend_message(b"S"))
    extended_rows = []
    described_parameters = None
    while True:
        kind, payload = read_message(stream)
        if kind == b"E":
            raise RuntimeError(f"65,535-parameter Bind failed: {payload!r}")
        if kind == b"t":
            if len(payload) < 2:
                raise RuntimeError("truncated ParameterDescription message")
            described_parameters = struct.unpack("!H", payload[:2])[0]
        if kind == b"D":
            extended_rows.append(text_data_row(payload))
        if kind == b"Z":
            break
    if described_parameters != count:
        raise RuntimeError(
            f"Statement Describe returned {described_parameters} parameters, expected {count}"
        )

    types = ",".join(["integer"] * count)
    arguments = ",".join(["NULL"] * (count - 1) + ["42"])
    sql = (
        f"PREPARE sql_wide ({types}) AS SELECT $65535; "
        f"EXECUTE sql_wide ({arguments}); "
        "SELECT cardinality(parameter_types) FROM pg_prepared_statements "
        "WHERE name = 'sql_wide'; DEALLOCATE sql_wide"
    )
    stream.sendall(frontend_message(b"Q", sql.encode() + b"\x00"))
    sql_rows = []
    while True:
        kind, payload = read_message(stream)
        if kind == b"E":
            raise RuntimeError(f"65,535-parameter SQL PREPARE failed: {payload!r}")
        if kind == b"D":
            sql_rows.append(text_data_row(payload))
        if kind == b"Z":
            break
    stream.close()
    return extended_rows, sql_rows


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--pg", type=int, required=True)
    parser.add_argument("--p3", type=int, required=True)
    parser.add_argument("--host", default="127.0.0.1")
    args = parser.parse_args()

    expected = ([["42"]], [["42"], ["65535"]])
    postgresql = parameter_capacity(args.host, args.pg)
    pos3ql = parameter_capacity(args.host, args.p3)
    if postgresql != expected or pos3ql != postgresql:
        print(f"PostgreSQL: {postgresql!r}")
        print(f"pos3ql:     {pos3ql!r}")
        return 1
    print("65,535 wire Parse/Describe/Bind and SQL PREPARE parameters match PostgreSQL")
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Compare PostgreSQL 18's complete query-result width over raw v3 wire."""

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
        messages = []
        while True:
            kind, payload = read_message(self.stream)
            messages.append((kind, payload))
            if kind == b"Z":
                return messages
            if kind == b"E" and operation == "startup":
                raise RuntimeError(f"startup failed: {payload!r}")

    def send_and_read(self, payload, operation):
        self.stream.sendall(payload)
        return self.read_until_ready(operation)

    def close(self):
        self.stream.close()


def row_summary(payload, binary):
    count = struct.unpack("!H", payload[:2])[0]
    at = 2
    first = None
    last = None
    for index in range(count):
        length = struct.unpack("!i", payload[at : at + 4])[0]
        at += 4
        value = None if length == -1 else payload[at : at + length]
        at += max(length, 0)
        if value is not None:
            value = struct.unpack("!i", value)[0] if binary[index] else value.decode()
        if index == 0:
            first = value
        last = value
    if at != len(payload):
        raise RuntimeError("DataRow has trailing bytes")
    return count, first, last


def error_summary(messages):
    errors = [error_fields(payload) for kind, payload in messages if kind == b"E"]
    return None if not errors else (errors[0].get("C"), errors[0].get("M"))


def probe(host, port):
    connection = Connection(host, port)
    try:
        columns = ",".join(str(index) for index in range(1664))
        scoped = f"SELECT {columns} FROM (VALUES (1)) AS v(x)"
        messages = connection.send_and_read(
            frontend_message(b"Q", scoped.encode() + b"\x00"), "scoped query"
        )
        scoped_rows = [
            row_summary(payload, [False] * 1664)
            for kind, payload in messages
            if kind == b"D"
        ]

        parse = frontend_message(b"P", b"wide\x00SELECT " + columns.encode() + b"\x00\x00\x00")
        describe = frontend_message(b"D", b"Swide\x00")
        messages = connection.send_and_read(
            parse + describe + frontend_message(b"S"), "statement describe"
        )
        describe_counts = [
            struct.unpack("!H", payload[:2])[0]
            for kind, payload in messages
            if kind == b"T"
        ]

        binary = [(index % 2) == 1 for index in range(1664)]
        result_formats = b"".join(struct.pack("!h", int(code)) for code in binary)
        bind_payload = (
            b"\x00wide\x00"
            + struct.pack("!H", 0)
            + struct.pack("!H", 0)
            + struct.pack("!H", len(binary))
            + result_formats
        )
        messages = connection.send_and_read(
            frontend_message(b"B", bind_payload)
            + frontend_message(b"E", b"\x00\x00\x00\x00\x00")
            + frontend_message(b"S"),
            "wide Bind/Execute",
        )
        extended_rows = [
            row_summary(payload, binary) for kind, payload in messages if kind == b"D"
        ]
        extended_error = error_summary(messages)

        ordered_keys = ",".join(f"x+{index}" for index in range(129))
        ordered = f"SELECT array_agg(x ORDER BY {ordered_keys}) FROM (VALUES (1)) AS v(x)"
        messages = connection.send_and_read(
            frontend_message(b"Q", ordered.encode() + b"\x00"), "ordered aggregate"
        )
        ordered_rows = [
            row_summary(payload, [False]) for kind, payload in messages if kind == b"D"
        ]

        too_wide = f"SELECT {columns},1664"
        messages = connection.send_and_read(
            frontend_message(b"Q", too_wide.encode() + b"\x00"), "too-wide query"
        )
        too_wide_error = error_summary(messages)
        return (
            scoped_rows,
            describe_counts,
            extended_rows,
            extended_error,
            ordered_rows,
            too_wide_error,
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
        [(1664, "0", "1663")],
        [1664],
        [(1664, "0", 1663)],
        None,
        [(1, "{1}", "{1}")],
        ("54011", "target lists can have at most 1664 entries"),
    )
    postgresql = probe(args.host, args.pg)
    pos3ql = probe(args.host, args.p3)
    if postgresql != expected or pos3ql != postgresql:
        print(f"expected:   {expected!r}")
        print(f"PostgreSQL: {postgresql!r}")
        print(f"pos3ql:     {pos3ql!r}")
        return 1
    print("1,664-column query, Describe, and per-column Bind formats match PostgreSQL")
    return 0


if __name__ == "__main__":
    sys.exit(main())

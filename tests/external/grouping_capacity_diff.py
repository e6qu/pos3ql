#!/usr/bin/env python3
"""Compare PostgreSQL 18 grouping widths and exact limit errors over v3 wire."""

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
        while True:
            kind, payload = read_message(self.stream)
            if kind == b"E":
                raise RuntimeError(f"startup failed: {payload!r}")
            if kind == b"Z":
                break

    def query(self, sql):
        self.stream.sendall(frontend_message(b"Q", sql.encode() + b"\x00"))
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
                return error if error is not None else rows

    def close(self):
        self.stream.close()


def cases():
    terms = [f"x+{index}" for index in range(1665)]
    semantic_terms = ["x", "v.x"] + [f"x+{index}" for index in range(1, 1663)]
    sets = ",".join("()" for _ in range(4097))
    cube = [f"x+{index}" for index in range(13)]
    grouping = [f"x+{index}" for index in range(32)]
    high_terms = ",".join(terms[:65])
    return [
        (
            "1,664 flat grouping expressions",
            "SELECT count(*) FROM (SELECT x+0 FROM (VALUES (1)) v(x) GROUP BY "
            + ",".join(terms[:1664])
            + ") grouped",
            [["1"]],
        ),
        (
            "1,665 visible and hidden target entries",
            "SELECT count(*) FROM (VALUES (1)) v(x) GROUP BY " + ",".join(terms[:1664]),
            ("54011", "target lists can have at most 1664 entries"),
        ),
        (
            "semantically duplicate grouping expressions",
            "SELECT count(*) FROM (SELECT count(*) FROM (VALUES (1)) v(x) GROUP BY "
            + ",".join(semantic_terms)
            + ") grouped",
            [["1"]],
        ),
        (
            "4,096 expanded grouping sets",
            "SELECT count(*) FROM (SELECT count(*) FROM (VALUES (1)) v(x) "
            "GROUP BY GROUPING SETS (" + ",".join("()" for _ in range(4096)) + ")) grouped",
            [["4096"]],
        ),
        (
            "4,097 expanded grouping sets",
            "SELECT count(*) FROM (VALUES (1)) v(x) GROUP BY GROUPING SETS (" + sets + ")",
            ("54001", "too many grouping sets present (maximum 4096)"),
        ),
        (
            "12-element CUBE",
            "SELECT count(*) FROM (SELECT count(*) FROM (VALUES (1)) v(x) GROUP BY CUBE ("
            + ",".join(cube[:12])
            + ")) grouped",
            [["4096"]],
        ),
        (
            "13-element CUBE",
            "SELECT count(*) FROM (VALUES (1)) v(x) GROUP BY CUBE (" + ",".join(cube) + ")",
            ("54011", "CUBE is limited to 12 elements"),
        ),
        (
            "grouping set above bit 63",
            "SELECT grouping(x+64), count(*) FROM (VALUES (1)) v(x) "
            "GROUP BY GROUPING SETS ((" + high_terms + "),(x+0)) ORDER BY 1",
            [["0", "1"], ["1", "1"]],
        ),
        (
            "31 GROUPING arguments",
            "SELECT grouping(" + ",".join(grouping[:31]) + ") FROM (VALUES (1)) v(x) GROUP BY "
            + ",".join(grouping[:31]),
            [["0"]],
        ),
        (
            "32 GROUPING arguments",
            "SELECT grouping(" + ",".join(grouping) + ") FROM (VALUES (1)) v(x) GROUP BY "
            + ",".join(grouping),
            ("54023", "GROUPING must have fewer than 32 arguments"),
        ),
    ]


def run(host, port, probes):
    connection = Connection(host, port)
    try:
        return [(name, connection.query(sql)) for name, sql, _ in probes]
    finally:
        connection.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--pg", type=int, required=True)
    parser.add_argument("--p3", type=int, required=True)
    parser.add_argument("--host", default="127.0.0.1")
    args = parser.parse_args()

    probes = cases()
    expected = [(name, result) for name, _, result in probes]
    postgresql = run(args.host, args.pg, probes)
    pos3ql = run(args.host, args.p3, probes)
    if postgresql != expected or pos3ql != postgresql:
        print(f"expected:   {expected!r}")
        print(f"PostgreSQL: {postgresql!r}")
        print(f"pos3ql:     {pos3ql!r}")
        return 1
    print("grouping widths, high bits, and exact PostgreSQL limit errors match")
    return 0


if __name__ == "__main__":
    sys.exit(main())

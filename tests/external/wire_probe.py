#!/usr/bin/env python3
"""Raw wire-protocol probes against a running pos3ql server.

Python stdlib only. Checks the parts of the PostgreSQL frontend/backend
protocol that a cooperative client like psql never exercises: SSL/GSSENC
probes, protocol version negotiation (3.0 / 3.2 / unknown minor),
BackendKeyData cancel-key lengths, empty queries, oversized messages,
unknown message types, and CancelRequest handling.
"""

import os
import socket
import struct
import sys

HOST = os.environ.get("POS3QL_HOST", "127.0.0.1")
PORT = int(os.environ.get("POS3QL_PORT", "5433"))

failures = []


def check(name, cond, detail=""):
    if cond:
        print(f"  ok  {name}")
    else:
        print(f"FAIL  {name} {detail}")
        failures.append(name)


def connect():
    s = socket.create_connection((HOST, PORT), timeout=5)
    s.settimeout(5)
    return s


def startup_payload(minor, user=b"postgres", parameters=()):
    body = struct.pack("!i", (3 << 16) | minor)
    body += b"user\x00" + user + b"\x00"
    for key, value in parameters:
        body += key.encode() + b"\x00" + value.encode() + b"\x00"
    body += b"\x00"
    return struct.pack("!i", len(body) + 4) + body


def read_message(s):
    header = recv_exact(s, 5)
    mtype = header[0:1]
    (length,) = struct.unpack("!i", header[1:5])
    payload = recv_exact(s, length - 4)
    return mtype, payload


def recv_exact(s, n):
    buf = b""
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("EOF")
        buf += chunk
    return buf


def frontend_message(kind, payload=b""):
    return kind + struct.pack("!i", len(payload) + 4) + payload


def standby_status(end_lsn, reply_requested=False):
    return frontend_message(
        b"d",
        b"r" + struct.pack("!QQQQB", end_lsn, end_lsn, end_lsn, 0, int(reply_requested)),
    )


def simple_query(s, text):
    s.sendall(frontend_message(b"Q", text.encode() + b"\x00"))
    out = []
    while True:
        item = read_message(s)
        out.append(item)
        if item[0] == b"Z":
            return out


def row_description_type_oids(payload):
    (count,) = struct.unpack("!h", payload[:2])
    at = 2
    oids = []
    for _ in range(count):
        end = payload.index(b"\x00", at)
        at = end + 1
        _, _, oid, _, _, _ = struct.unpack("!ihihih", payload[at : at + 18])
        oids.append(oid)
        at += 18
    return oids


def row_description_formats(payload):
    (count,) = struct.unpack("!h", payload[:2])
    at = 2
    formats = []
    for _ in range(count):
        end = payload.index(b"\x00", at)
        at = end + 1
        formats.append(struct.unpack("!h", payload[at + 16 : at + 18])[0])
        at += 18
    return formats


def row_description_type_modifiers(payload):
    (count,) = struct.unpack("!h", payload[:2])
    at = 2
    type_modifiers = []
    for _ in range(count):
        end = payload.index(b"\x00", at)
        at = end + 1
        type_modifiers.append(struct.unpack("!i", payload[at + 12 : at + 16])[0])
        at += 18
    return type_modifiers


def start_extended(s, text, max_rows=0):
    parse = frontend_message(b"P", b"\x00" + text.encode() + b"\x00\x00\x00")
    bind = frontend_message(b"B", b"\x00\x00\x00\x00\x00\x00\x00\x00")
    execute = frontend_message(b"E", b"\x00" + struct.pack("!i", max_rows))
    s.sendall(parse + bind + execute + frontend_message(b"H"))


def drain_startup(s):
    """Reads messages until ReadyForQuery; returns dict of interesting ones."""
    seen = {}
    while True:
        mtype, payload = read_message(s)
        seen.setdefault(mtype, []).append(payload)
        if mtype == b"Z":
            return seen


def test_ssl_and_gssenc_probes():
    for code, name in [(80877103, "SSLRequest"), (80877104, "GSSENCRequest")]:
        s = connect()
        s.sendall(struct.pack("!ii", 8, code))
        answer = recv_exact(s, 1)
        check(f"{name} answered 'N'", answer == b"N", repr(answer))
        # The connection must still accept a normal startup afterwards.
        s.sendall(startup_payload(0))
        seen = drain_startup(s)
        check(f"startup after {name} reaches ReadyForQuery", b"Z" in seen)
        s.close()


def test_protocol_30():
    s = connect()
    s.sendall(startup_payload(0))
    seen = drain_startup(s)
    check("3.0: AuthenticationOk", b"R" in seen and seen[b"R"][0] == b"\x00\x00\x00\x00")
    check("3.0: no NegotiateProtocolVersion", b"v" not in seen)
    key_data = seen[b"K"][0]
    check("3.0: BackendKeyData has 4-byte key", len(key_data) == 8, f"len={len(key_data)}")
    params = {}
    for p in seen.get(b"S", []):
        k, v = p.rstrip(b"\x00").split(b"\x00", 1)
        params[k] = v
    check("3.0: server_encoding UTF8", params.get(b"server_encoding") == b"UTF8")
    check(
        "3.0: standard_conforming_strings on",
        params.get(b"standard_conforming_strings") == b"on",
    )
    s.close()


def test_protocol_32():
    s = connect()
    s.sendall(startup_payload(2))
    seen = drain_startup(s)
    check("3.2: AuthenticationOk", b"R" in seen)
    check("3.2: no NegotiateProtocolVersion", b"v" not in seen)
    key_data = seen[b"K"][0]
    check(
        "3.2: BackendKeyData key is 4..256 bytes and longer than 3.0's",
        8 < len(key_data) <= 260,
        f"len={len(key_data)}",
    )
    s.close()


def test_unknown_minor_negotiates():
    s = connect()
    s.sendall(startup_payload(7))  # 3.7 does not exist
    seen = drain_startup(s)
    check("3.7: NegotiateProtocolVersion sent", b"v" in seen)
    if b"v" in seen:
        newest, n_opts = struct.unpack("!ii", seen[b"v"][0][:8])
        check("3.7: negotiated down to 3.2", newest == 2, f"newest={newest}")
        check("3.7: no unknown options reported", n_opts == 0)
    check("3.7: startup still completes", b"Z" in seen)
    s.close()


def test_unknown_protocol_option():
    s = connect()
    body = struct.pack("!i", 3 << 16)
    body += b"user\x00postgres\x00_pq_.made_up_option\x00yes\x00\x00"
    s.sendall(struct.pack("!i", len(body) + 4) + body)
    seen = drain_startup(s)
    check("_pq_ option: NegotiateProtocolVersion sent", b"v" in seen)
    if b"v" in seen:
        payload = seen[b"v"][0]
        _, n_opts = struct.unpack("!ii", payload[:8])
        check("_pq_ option: option named back", n_opts == 1 and b"_pq_.made_up_option" in payload)
    s.close()


def test_simple_query_and_empty():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)

    def q(text):
        body = text.encode() + b"\x00"
        s.sendall(b"Q" + struct.pack("!i", len(body) + 4) + body)
        out = []
        while True:
            mtype, payload = read_message(s)
            out.append((mtype, payload))
            if mtype == b"Z":
                return out

    out = q("SELECT 1")
    types = [m for m, _ in out]
    check("simple query: T D C Z", types == [b"T", b"D", b"C", b"Z"], types)
    datarow = out[1][1]
    check("simple query: value is text '1'", datarow == b"\x00\x01\x00\x00\x00\x011")

    out = q("")
    types = [m for m, _ in out]
    check("empty query: EmptyQueryResponse", types == [b"I", b"Z"], types)

    out = q("SELECT 'a'; SELECT 'b'")
    types = [m for m, _ in out]
    check(
        "multi-statement: two result sets, one ReadyForQuery",
        types == [b"T", b"D", b"C", b"T", b"D", b"C", b"Z"],
        types,
    )

    # No user name → error at startup is covered elsewhere; here: an
    # unknown frontend message type must produce an error and a close.
    s.sendall(b"@" + struct.pack("!i", 4))
    mtype, _ = read_message(s)
    check("unknown message type: ErrorResponse", mtype == b"E")
    tail = s.recv(1)
    check("unknown message type: connection closed", tail == b"")
    s.close()


def test_binary_cursor_fetches_binary_rows():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    out = simple_query(
        s,
        "BEGIN; DECLARE binary_probe BINARY CURSOR FOR SELECT 42::int4 AS answer; "
        "FETCH ALL FROM binary_probe; COMMIT",
    )
    description = next((payload for kind, payload in out if kind == b"T"), None)
    row = next((payload for kind, payload in out if kind == b"D"), None)
    check(
        "binary cursor: FETCH describes binary int4",
        description is not None
        and row_description_type_oids(description) == [23]
        and row_description_formats(description) == [1],
        description,
    )
    check(
        "binary cursor: FETCH returns network-order int4",
        row == b"\x00\x01\x00\x00\x00\x04\x00\x00\x00*",
        row,
    )
    s.close()


def test_binary_cursor_preserves_type_modifier():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    out = simple_query(
        s,
        "BEGIN; DECLARE type_modifier_probe BINARY CURSOR FOR "
        "SELECT 'abc'::varchar(3) AS value; FETCH ALL FROM type_modifier_probe; COMMIT",
    )
    description = next((payload for kind, payload in out if kind == b"T"), None)
    row = next((payload for kind, payload in out if kind == b"D"), None)
    check(
        "binary cursor: FETCH preserves varchar typmod",
        description is not None
        and row_description_type_oids(description) == [1043]
        and row_description_type_modifiers(description) == [7]
        and row_description_formats(description) == [1],
        description,
    )
    check("binary cursor: FETCH preserves varchar bytes", row == b"\x00\x01\x00\x00\x00\x03abc", row)
    s.close()


def test_binary_cursor_preserves_catalog_identity():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    out = simple_query(
        s,
        "BEGIN; DECLARE catalog_identity_probe BINARY CURSOR FOR "
        "SELECT '+(integer,integer)'::regoperator AS value; "
        "FETCH ALL FROM catalog_identity_probe; COMMIT",
    )
    description = next((payload for kind, payload in out if kind == b"T"), None)
    row = next((payload for kind, payload in out if kind == b"D"), None)
    check(
        "binary cursor: FETCH preserves regoperator identity",
        description is not None
        and row_description_type_oids(description) == [2204]
        and row_description_formats(description) == [1],
        description,
    )
    check(
        "binary cursor: FETCH returns the operator OID",
        row == b"\x00\x01\x00\x00\x00\x04\x00\x00\x02'",
        row,
    )
    s.close()


def test_binary_portal_preserves_catalog_identity():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    parse = frontend_message(
        b"P",
        b"catalog_identity_statement\x00SELECT $1::regoperator AS value\x00"
        + struct.pack("!hi", 1, 2204),
    )
    bind = frontend_message(
        b"B",
        b"catalog_identity_portal\x00catalog_identity_statement\x00"
        + struct.pack("!hhh", 1, 1, 1)
        + struct.pack("!i", 4)
        + struct.pack("!i", 551)
        + struct.pack("!hh", 1, 1),
    )
    describe = frontend_message(b"D", b"Pcatalog_identity_portal\x00")
    execute = frontend_message(b"E", b"catalog_identity_portal\x00\x00\x00\x00\x00")
    s.sendall(parse + bind + describe + execute + frontend_message(b"S"))
    out = []
    while True:
        item = read_message(s)
        out.append(item)
        if item[0] == b"Z":
            break
    description = next((payload for kind, payload in out if kind == b"T"), None)
    row = next((payload for kind, payload in out if kind == b"D"), None)
    check(
        "binary portal: Bind retains regoperator result identity",
        description is not None
        and row_description_type_oids(description) == [2204]
        and row_description_formats(description) == [1],
        out,
    )
    check(
        "binary portal: Execute returns the bound operator OID",
        row == b"\x00\x01\x00\x00\x00\x04\x00\x00\x02'",
        row,
    )
    s.close()


def test_bind_rejects_invalid_format_codes_and_lengths():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)

    parse = frontend_message(b"P", b"\x00SELECT $1\x00\x00\x00")
    invalid_format_bind = frontend_message(
        b"B",
        b"\x00\x00" + struct.pack("!hhh", 1, 2, 1) + struct.pack("!i", 1) + b"1" + struct.pack("!h", 0),
    )
    s.sendall(parse + invalid_format_bind + frontend_message(b"S"))
    out = [read_message(s), read_message(s), read_message(s)]
    check(
        "Bind rejects an unsupported parameter format code",
        [kind for kind, _ in out] == [b"1", b"E", b"Z"] and has_sqlstate(out, "08P01"),
        out,
    )

    parse = frontend_message(b"P", b"\x00SELECT $1\x00\x00\x00")
    invalid_length_bind = frontend_message(
        b"B",
        b"\x00\x00" + struct.pack("!hhh", 0, 1, 1) + struct.pack("!i", -2) + struct.pack("!h", 0),
    )
    s.sendall(parse + invalid_length_bind + frontend_message(b"S"))
    out = [read_message(s), read_message(s), read_message(s)]
    check(
        "Bind rejects a parameter length other than -1 or a byte count",
        [kind for kind, _ in out] == [b"1", b"E", b"Z"] and has_sqlstate(out, "08P01"),
        out,
    )
    s.close()


def test_bind_rejects_mismatched_result_format_count():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)

    parse = frontend_message(b"P", b"\x00SELECT 1\x00\x00\x00")
    mismatched_formats = frontend_message(
        b"B",
        b"\x00\x00"
        + struct.pack("!hhhhh", 0, 0, 2, 0, 1),
    )
    s.sendall(parse + mismatched_formats + frontend_message(b"S"))
    out = [read_message(s), read_message(s), read_message(s)]
    check(
        "Bind rejects result formats that do not match query columns",
        [kind for kind, _ in out] == [b"1", b"E", b"Z"]
        and has_sqlstate(out, "08P01")
        and b"bind message has 2 result formats but query has 1 columns" in out[1][1],
        out,
    )

    parse = frontend_message(b"P", b"\x00SELECT 1::int4, 2::int4\x00\x00\x00")
    matching_formats = frontend_message(
        b"B",
        b"\x00\x00"
        + struct.pack("!hhhhh", 0, 0, 2, 1, 0),
    )
    execute = frontend_message(b"E", b"\x00\x00\x00\x00\x00")
    s.sendall(parse + matching_formats + execute + frontend_message(b"S"))
    out = []
    while True:
        item = read_message(s)
        out.append(item)
        if item[0] == b"Z":
            break
    row = next((payload for kind, payload in out if kind == b"D"), None)
    check(
        "Bind keeps each validated result format",
        row == b"\x00\x02\x00\x00\x00\x04\x00\x00\x00\x01\x00\x00\x00\x012",
        out,
    )
    s.close()


def test_portal_describe_preserves_type_modifier():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    parse = frontend_message(b"P", b"\x00SELECT 'abc'::varchar(3) AS value\x00\x00\x00")
    bind = frontend_message(
        b"B",
        b"typed_portal\x00\x00" + struct.pack("!hhh", 0, 0, 1) + struct.pack("!h", 1),
    )
    describe = frontend_message(b"D", b"Ptyped_portal\x00")
    s.sendall(parse + bind + describe + frontend_message(b"S"))
    out = []
    while True:
        item = read_message(s)
        out.append(item)
        if item[0] == b"Z":
            break
    description = next((payload for kind, payload in out if kind == b"T"), None)
    check(
        "portal Describe preserves varchar typmod and result format",
        description is not None
        and row_description_type_oids(description) == [1043]
        and row_description_type_modifiers(description) == [7]
        and row_description_formats(description) == [1],
        out,
    )
    s.close()


def test_builtin_function_result_types_and_binary_json():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    parse = frontend_message(
        b"P",
        b"\x00SELECT jsonb_set('{\"a\": 1}'::jsonb, '{a}', '2'::jsonb), "
        b"json_strip_nulls('{\"a\": null}'::json)\x00\x00\x00",
    )
    bind = frontend_message(
        b"B",
        b"\x00\x00" + struct.pack("!hhh", 0, 0, 2) + struct.pack("!hh", 1, 1),
    )
    describe = frontend_message(b"D", b"P\x00")
    execute = frontend_message(b"E", b"\x00\x00\x00\x00\x00")
    s.sendall(parse + bind + describe + execute + frontend_message(b"S"))
    out = []
    while True:
        item = read_message(s)
        out.append(item)
        if item[0] == b"Z":
            break
    description = next((payload for kind, payload in out if kind == b"T"), None)
    row = next((payload for kind, payload in out if kind == b"D"), None)
    check(
        "typed JSON functions preserve Describe OIDs and binary formats",
        description is not None
        and row_description_type_oids(description) == [3802, 114]
        and row_description_formats(description) == [1, 1],
        out,
    )
    expected = (
        b"\x00\x02"
        + struct.pack("!i", len(b'\x01{\"a\": 2}'))
        + b'\x01{\"a\": 2}'
        + struct.pack("!i", len(b"{}"))
        + b"{}"
    )
    check("typed JSON functions preserve binary result bytes", row == expected, row)

    parse = frontend_message(b"P", b"\x00SELECT pg_typeof(7)\x00\x00\x00")
    bind = frontend_message(
        b"B",
        b"\x00\x00" + struct.pack("!hhh", 0, 0, 1) + struct.pack("!h", 1),
    )
    describe = frontend_message(b"D", b"P\x00")
    execute = frontend_message(b"E", b"\x00\x00\x00\x00\x00")
    s.sendall(parse + bind + describe + execute + frontend_message(b"S"))
    out = []
    while True:
        item = read_message(s)
        out.append(item)
        if item[0] == b"Z":
            break
    description = next((payload for kind, payload in out if kind == b"T"), None)
    row = next((payload for kind, payload in out if kind == b"D"), None)
    check(
        "pg_typeof preserves regtype Describe and binary format",
        description is not None
        and row_description_type_oids(description) == [2206]
        and row_description_formats(description) == [1],
        out,
    )
    check(
        "pg_typeof preserves referenced OID in binary result",
        row == b"\x00\x01\x00\x00\x00\x04\x00\x00\x00\x17",
        row,
    )
    simple_query(
        s,
        "CREATE TABLE wire_regtype_value (value regtype DEFAULT 'integer'::regtype); "
        "INSERT INTO wire_regtype_value DEFAULT VALUES",
    )
    parse = frontend_message(b"P", b"\x00SELECT value FROM wire_regtype_value\x00\x00\x00")
    bind = frontend_message(
        b"B",
        b"\x00\x00" + struct.pack("!hhh", 0, 0, 1) + struct.pack("!h", 1),
    )
    s.sendall(parse + bind + describe + execute + frontend_message(b"S"))
    out = []
    while True:
        item = read_message(s)
        out.append(item)
        if item[0] == b"Z":
            break
    description = next((payload for kind, payload in out if kind == b"T"), None)
    row = next((payload for kind, payload in out if kind == b"D"), None)
    check(
        "stored regtype preserves Describe and binary result bytes",
        description is not None
        and row_description_type_oids(description) == [2206]
        and row_description_formats(description) == [1]
        and row == b"\x00\x01\x00\x00\x00\x04\x00\x00\x00\x17",
        out,
    )
    simple_query(
        s,
        "CREATE ROLE wire_catalog_role; "
        "CREATE SCHEMA wire_catalog_schema; "
        "CREATE TABLE wire_catalog_reference (id integer); "
        "CREATE TABLE wire_catalog_value ("
        "  relation_value regclass DEFAULT 'wire_catalog_reference'::regclass, "
        "  role_value regrole DEFAULT 'wire_catalog_role'::regrole, "
        "  schema_value regnamespace DEFAULT 'wire_catalog_schema'::regnamespace, "
        "  operator_value regoperator DEFAULT '+(integer,integer)'::regoperator"
        "); "
        "INSERT INTO wire_catalog_value DEFAULT VALUES",
    )
    relation_oid = int(
        first_text_row(
            simple_query(s, "SELECT oid FROM pg_class WHERE relname = 'wire_catalog_reference'")
        )
    )
    parse = frontend_message(b"P", b"\x00SELECT relation_value FROM wire_catalog_value\x00\x00\x00")
    s.sendall(parse + bind + describe + execute + frontend_message(b"S"))
    out = []
    while True:
        item = read_message(s)
        out.append(item)
        if item[0] == b"Z":
            break
    description = next((payload for kind, payload in out if kind == b"T"), None)
    row = next((payload for kind, payload in out if kind == b"D"), None)
    check(
        "stored regclass preserves Describe and binary result bytes",
        description is not None
        and row_description_type_oids(description) == [2205]
        and row_description_formats(description) == [1]
        and row == b"\x00\x01\x00\x00\x00\x04" + struct.pack("!i", relation_oid),
        out,
    )
    parse = frontend_message(b"P", b"\x00SELECT operator_value FROM wire_catalog_value\x00\x00\x00")
    s.sendall(parse + bind + describe + execute + frontend_message(b"S"))
    out = []
    while True:
        item = read_message(s)
        out.append(item)
        if item[0] == b"Z":
            break
    description = next((payload for kind, payload in out if kind == b"T"), None)
    row = next((payload for kind, payload in out if kind == b"D"), None)
    check(
        "stored regoperator preserves Describe and binary result bytes",
        description is not None
        and row_description_type_oids(description) == [2204]
        and row_description_formats(description) == [1]
        and row == b"\x00\x01\x00\x00\x00\x04" + struct.pack("!i", 551),
        out,
    )
    s.close()


def test_generate_series_result_types_and_binary_format():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)

    def run(text, expected_oid):
        parse = frontend_message(b"P", b"\x00" + text.encode() + b"\x00\x00\x00")
        bind = frontend_message(b"B", b"\x00\x00" + struct.pack("!hhh", 0, 0, 1) + struct.pack("!h", 1))
        describe = frontend_message(b"D", b"P\x00")
        execute = frontend_message(b"E", b"\x00\x00\x00\x00\x00")
        s.sendall(parse + bind + describe + execute + frontend_message(b"S"))
        out = []
        while True:
            item = read_message(s)
            out.append(item)
            if item[0] == b"Z":
                break
        description = next((payload for kind, payload in out if kind == b"T"), None)
        rows = [payload for kind, payload in out if kind == b"D"]
        check(
            f"generate_series {expected_oid}: binary Result metadata",
            description is not None
            and row_description_type_oids(description) == [expected_oid]
            and row_description_formats(description) == [1]
            and len(rows) == 3,
            out,
        )

    run(
        "SELECT generate_series('2000-01-01'::timestamp, '2000-01-03'::timestamp, '1 day'::interval)",
        1114,
    )
    run("SELECT generate_series(1.5::numeric, 2.5::numeric, 0.5::numeric)", 1700)
    s.close()


def test_parse_rejects_invalid_type_counts_and_trailing_bytes():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)

    negative_count = frontend_message(b"P", b"\x00SELECT 1\x00" + struct.pack("!h", -1))
    s.sendall(negative_count + frontend_message(b"S"))
    out = [read_message(s), read_message(s)]
    check(
        "Parse rejects a negative parameter-type count",
        [kind for kind, _ in out] == [b"E", b"Z"] and has_sqlstate(out, "08P01"),
        out,
    )

    trailing = frontend_message(b"P", b"\x00SELECT 1\x00\x00\x00x")
    s.sendall(trailing + frontend_message(b"S"))
    out = [read_message(s), read_message(s)]
    check(
        "Parse rejects trailing bytes",
        [kind for kind, _ in out] == [b"E", b"Z"] and has_sqlstate(out, "08P01"),
        out,
    )
    s.close()


def test_logical_replication_simple_query_mode():
    s = connect()
    s.sendall(startup_payload(0, parameters=(("replication", "database"),)))
    drain_startup(s)

    out = simple_query(s, "SELECT 1")
    check(
        "logical replication accepts ordinary simple SQL",
        [m for m, _ in out] == [b"T", b"D", b"C", b"Z"],
        [m for m, _ in out],
    )

    out = simple_query(s, "IDENTIFY_SYSTEM")
    types = [m for m, _ in out]
    check("logical IDENTIFY_SYSTEM result framing", types == [b"T", b"D", b"C", b"Z"], types)
    if types == [b"T", b"D", b"C", b"Z"]:
        check(
            "logical IDENTIFY_SYSTEM timeline is int8",
            row_description_type_oids(out[0][1]) == [25, 20, 25, 25],
            row_description_type_oids(out[0][1]),
        )
    s.close()


def test_pgoutput_startup_options_and_default_text_tuples():
    setup = connect()
    setup.sendall(startup_payload(0))
    drain_startup(setup)
    simple_query(
        setup,
        "DROP PUBLICATION IF EXISTS \"wire, replication\"; "
        "DROP PUBLICATION IF EXISTS wire_replication_pub; "
        "DROP TABLE IF EXISTS wire_replication_two; "
        "DROP TABLE IF EXISTS wire_replication; "
        "DROP TYPE IF EXISTS wire_replication_state; "
        "CREATE TYPE wire_replication_state AS ENUM ('ready'); "
        "CREATE TABLE wire_replication (id integer); "
        "CREATE TABLE wire_replication_two (id integer, state wire_replication_state); "
        "CREATE PUBLICATION wire_replication_pub FOR TABLE wire_replication; "
        "CREATE PUBLICATION \"wire, replication\" FOR TABLE wire_replication_two",
    )

    stream = connect()
    stream.sendall(startup_payload(0, parameters=(("replication", "database"),)))
    drain_startup(stream)
    simple_query(
        stream,
        "CREATE_REPLICATION_SLOT wire_replication_slot LOGICAL pgoutput NOEXPORT_SNAPSHOT",
    )
    stream.sendall(
        frontend_message(
            b"Q",
            b"START_REPLICATION SLOT wire_replication_slot LOGICAL 0/0 "
            b"(proto_version '1', publication_names 'wire_replication_pub,\"wire, replication\"')\x00",
        )
    )
    kind, payload = read_message(stream)
    check("pgoutput START_REPLICATION enters CopyBoth", kind == b"W", (kind, payload))

    # A client may ping a quiet publisher by requesting an immediate status
    # reply. The server must return the replication-protocol `k` envelope,
    # not an empty CopyData frame or a pgoutput plugin message.
    stream.sendall(standby_status(0, reply_requested=True))
    keepalive = None
    for _ in range(64):
        kind, payload = read_message(stream)
        if kind == b"d" and len(payload) == 18 and payload[:1] == b"k":
            keepalive = payload
            break
        if kind == b"d" and len(payload) > 25 and payload[:1] == b"w" and payload[25:26] == b"C":
            stream.sendall(standby_status(struct.unpack("!Q", payload[9:17])[0]))
    keepalive_end_lsn = struct.unpack("!Q", keepalive[1:9])[0] if keepalive else None
    check(
        "standby reply request receives a primary keepalive",
        keepalive is not None
        and keepalive[-1:] == b"\x01"
        and keepalive_end_lsn is not None,
        keepalive,
    )

    simple_query(
        setup,
        "BEGIN; INSERT INTO wire_replication VALUES (42); "
        "INSERT INTO wire_replication_two VALUES (7, 'ready'); COMMIT",
    )
    inserts = []
    plugins = []
    for _ in range(128):
        kind, payload = read_message(stream)
        if kind == b"d" and len(payload) > 25 and payload[:1] == b"w":
            plugins.append(payload[25:26])
            if payload[25:26] == b"I":
                inserts.append(payload)
        if kind == b"d" and len(payload) > 25 and payload[:1] == b"w" and payload[25:26] == b"C":
            end_lsn = struct.unpack("!Q", payload[9:17])[0]
            stream.sendall(standby_status(end_lsn))
            if len(inserts) == 2:
                break
    check(
        "pgoutput publication union emits both text tuples exactly once",
        len(inserts) == 2
        and any(payload.endswith(b"42") for payload in inserts)
        and any(payload.endswith(b"ready") and b"\x00\x00\x00\x01" in payload for payload in inserts),
        inserts,
    )
    check(
        "pgoutput Type precedes Relation for enum columns",
        any(plugins[index:index + 2] == [b"Y", b"R"] for index in range(len(plugins) - 1)),
        plugins,
    )

    stream.close()

    # Versions 3 and 4 add optional in-progress / two-phase capabilities, but
    # retain the ordinary committed-transaction message flow negotiated here.
    for proto_version in (3, 4):
        stream = connect()
        stream.sendall(startup_payload(0, parameters=(("replication", "database"),)))
        drain_startup(stream)
        slot = f"wire_replication_v{proto_version}"
        simple_query(
            stream,
            f"CREATE_REPLICATION_SLOT {slot} LOGICAL pgoutput NOEXPORT_SNAPSHOT",
        )
        stream.sendall(
            frontend_message(
                b"Q",
                (
                    f"START_REPLICATION SLOT {slot} LOGICAL 0/0 "
                    f"(proto_version '{proto_version}', publication_names 'wire_replication_pub')"
                ).encode()
                + b"\x00",
            )
        )
        kind, payload = read_message(stream)
        check(
            f"pgoutput protocol v{proto_version} enters CopyBoth",
            kind == b"W",
            (kind, payload),
        )
        stream.close()
    setup.close()


def test_physical_replication_identify_system_mode():
    s = connect()
    s.sendall(startup_payload(0, parameters=(("replication", "true"),)))
    drain_startup(s)

    out = simple_query(s, "IDENTIFY_SYSTEM")
    types = [m for m, _ in out]
    check("physical IDENTIFY_SYSTEM result framing", types == [b"T", b"D", b"C", b"Z"], types)
    if types == [b"T", b"D", b"C", b"Z"]:
        check(
            "physical IDENTIFY_SYSTEM timeline is int8",
            row_description_type_oids(out[0][1]) == [25, 20, 25, 25],
            row_description_type_oids(out[0][1]),
        )

    out = simple_query(s, "SELECT 1")
    check(
        "physical replication does not accept ordinary SQL",
        [m for m, _ in out] == [b"E", b"Z"],
        [m for m, _ in out],
    )
    s.close()


def test_oversized_message_is_rejected():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    # Claim a Query far larger than any sane receive buffer.
    s.sendall(b"Q" + struct.pack("!i", 512 * 1024 * 1024))
    try:
        mtype, _ = read_message(s)
        closed = False
    except ConnectionError:
        closed = True
        mtype = None
    check(
        "oversized message: error or close, never a hang",
        closed or mtype in (b"E",),
        mtype,
    )
    s.close()


def test_startup_without_user():
    s = connect()
    body = struct.pack("!i", 3 << 16) + b"application_name\x00x\x00\x00"
    s.sendall(struct.pack("!i", len(body) + 4) + body)
    mtype, payload = read_message(s)
    check("startup without user: ErrorResponse", mtype == b"E")
    check("startup without user: SQLSTATE 28000", b"C28000" in payload, payload)
    s.close()


def test_cancel_request_closes_quietly():
    s = connect()
    s.sendall(struct.pack("!ii", 16, 80877102) + b"\x00" * 8)
    tail = s.recv(1)
    check("CancelRequest: closed without response", tail == b"")
    s.close()


def test_extended_copy():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    simple_query(
        s,
        "DROP TABLE IF EXISTS wire_copy; "
        "CREATE TABLE wire_copy (id integer, note text)",
    )

    start_extended(s, "COPY wire_copy FROM STDIN", max_rows=1)
    before = [read_message(s), read_message(s), read_message(s)]
    check(
        "extended COPY IN: ParseComplete, BindComplete, CopyInResponse",
        [kind for kind, _ in before] == [b"1", b"2", b"G"],
        [kind for kind, _ in before],
    )

    # A pipelined Sync received in copy mode is ignored. CopyDone completes
    # the command, and the client must send a later Sync for ReadyForQuery.
    s.sendall(
        frontend_message(b"S")
        + frontend_message(b"d", b"1\tone")
        + frontend_message(b"d", b"\n2\ttwo\n")
        + frontend_message(b"c")
        + frontend_message(b"H")
    )
    kind, payload = read_message(s)
    check(
        "extended COPY IN: CopyDone returns command count",
        kind == b"C" and payload == b"COPY 2\x00",
        (kind, payload),
    )
    s.settimeout(0.1)
    try:
        unexpected = read_message(s)
    except (TimeoutError, socket.timeout):
        unexpected = None
    check(
        "extended COPY IN: no ReadyForQuery before post-COPY Sync",
        unexpected is None,
        unexpected,
    )
    s.settimeout(5)
    s.sendall(frontend_message(b"S"))
    check("extended COPY IN: Sync returns ReadyForQuery", read_message(s)[0] == b"Z")

    rows = simple_query(s, "SELECT id, note FROM wire_copy ORDER BY id")
    check(
        "extended COPY IN: split CopyData chunks stored both rows",
        [kind for kind, _ in rows] == [b"T", b"D", b"D", b"C", b"Z"],
        [kind for kind, _ in rows],
    )

    start_extended(s, "COPY wire_copy FROM STDIN")
    while read_message(s)[0] != b"G":
        pass
    s.sendall(
        frontend_message(b"f", b"probe stopped the copy\x00")
        + frontend_message(b"H")
    )
    kind, payload = read_message(s)
    check(
        "extended COPY IN: CopyFail preserves reason and SQLSTATE",
        kind == b"E"
        and b"C57014\x00" in payload
        and b"probe stopped the copy" in payload,
        (kind, payload),
    )
    s.sendall(frontend_message(b"S"))
    kind, payload = read_message(s)
    check(
        "extended COPY IN: CopyFail recovers at Sync",
        kind == b"Z" and payload == b"I",
        (kind, payload),
    )

    start_extended(s, "COPY wire_copy TO STDOUT", max_rows=1)
    s.sendall(frontend_message(b"S"))
    out = []
    while True:
        item = read_message(s)
        out.append(item)
        if item[0] == b"Z":
            break
    kinds = [kind for kind, _ in out]
    check(
        "extended COPY OUT: max_rows does not suspend COPY",
        kinds == [b"1", b"2", b"H", b"d", b"d", b"c", b"C", b"Z"],
        kinds,
    )
    check(
        "extended COPY OUT: streams both rows",
        b"".join(payload for kind, payload in out if kind == b"d")
        == b"1\tone\n2\ttwo\n",
    )
    s.close()


def binary_array(element_oid, values):
    body = struct.pack("!iii", 1, int(any(value is None for value in values)), element_oid)
    body += struct.pack("!ii", len(values), 1)
    for value in values:
        if value is None:
            body += struct.pack("!i", -1)
        else:
            body += struct.pack("!i", len(value)) + value
    return body


def binary_record(fields):
    body = struct.pack("!i", len(fields))
    for field_oid, value in fields:
        body += struct.pack("!ii", field_oid, -1 if value is None else len(value))
        if value is not None:
            body += value
    return body


def extended_binary_parameter(s, text, oid, value):
    parse = frontend_message(
        b"P", b"\x00" + text.encode() + b"\x00" + struct.pack("!hi", 1, oid)
    )
    encoded_value = struct.pack("!i", -1) if value is None else struct.pack("!i", len(value)) + value
    bind = frontend_message(
        b"B",
        b"\x00\x00" + struct.pack("!hhh", 1, 1, 1) + encoded_value + struct.pack("!h", 0),
    )
    execute = frontend_message(b"E", b"\x00\x00\x00\x00\x00")
    s.sendall(parse + bind + execute + frontend_message(b"S"))
    out = []
    while True:
        item = read_message(s)
        out.append(item)
        if item[0] == b"Z":
            return out


def extended_text_parameter(s, text, oid, value):
    parse = frontend_message(
        b"P", b"\x00" + text.encode() + b"\x00" + struct.pack("!hi", 1, oid)
    )
    encoded_value = struct.pack("!i", -1) if value is None else struct.pack("!i", len(value)) + value
    bind = frontend_message(
        b"B",
        b"\x00\x00" + struct.pack("!hhh", 1, 0, 1) + encoded_value + struct.pack("!h", 0),
    )
    execute = frontend_message(b"E", b"\x00\x00\x00\x00\x00")
    s.sendall(parse + bind + execute + frontend_message(b"S"))
    out = []
    while True:
        item = read_message(s)
        out.append(item)
        if item[0] == b"Z":
            return out


def first_text_row(messages):
    row = next((payload for kind, payload in messages if kind == b"D"), None)
    if row is None or row[:2] != b"\x00\x01":
        return None
    (length,) = struct.unpack("!i", row[2:6])
    return row[6 : 6 + length].decode() if length >= 0 and len(row) == 6 + length else None


def text_row_fields(payload):
    """Decode one text-format DataRow without assuming a one-column result."""
    if len(payload) < 2:
        return None
    (count,) = struct.unpack("!h", payload[:2])
    at = 2
    fields = []
    for _ in range(count):
        if at + 4 > len(payload):
            return None
        (length,) = struct.unpack("!i", payload[at : at + 4])
        at += 4
        if length == -1:
            fields.append(None)
            continue
        if length < 0 or at + length > len(payload):
            return None
        fields.append(payload[at : at + length].decode())
        at += length
    return fields if at == len(payload) else None


def has_sqlstate(messages, state):
    return any(kind == b"E" and b"C" + state.encode() + b"\x00" in payload for kind, payload in messages)


def test_row_trigger_body_over_raw_wire():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(
        s,
        "CREATE TABLE wire_trigger_target (id integer PRIMARY KEY, value integer); "
        "CREATE TABLE wire_trigger_audit (id integer, observed integer); "
        "CREATE TABLE wire_trigger_side (id integer PRIMARY KEY, value integer); "
        "INSERT INTO wire_trigger_side VALUES (1, 10), (2, 20); "
        "CREATE FUNCTION wire_trigger_fn() RETURNS trigger LANGUAGE plpgsql AS "
        "'BEGIN NEW.value := NEW.value + 1; "
        "IF NEW.id = 1 THEN UPDATE wire_trigger_side SET value = NEW.value WHERE id = NEW.id; "
        "ELSE DELETE FROM wire_trigger_side WHERE id = NEW.id; END IF; "
        "INSERT INTO wire_trigger_audit VALUES (NEW.id, NEW.value); RETURN NEW; END'; "
        "CREATE TRIGGER wire_trigger BEFORE INSERT ON wire_trigger_target "
        "FOR EACH ROW WHEN (NEW.value > 0) EXECUTE FUNCTION wire_trigger_fn(); "
        "INSERT INTO wire_trigger_target VALUES (1, 4), (2, 4)",
    )
    check("raw wire: trigger body setup succeeds", not any(kind == b"E" for kind, _ in setup), setup)
    check(
        "raw wire: trigger assignment is visible",
        first_text_row(simple_query(s, "SELECT value FROM wire_trigger_target")) == "5",
    )
    check(
        "raw wire: trigger-side insert is visible",
        first_text_row(simple_query(s, "SELECT observed FROM wire_trigger_audit")) == "5",
    )
    check(
        "raw wire: trigger-side update and delete are visible",
        first_text_row(simple_query(s, "SELECT value FROM wire_trigger_side WHERE id = 1")) == "5"
        and first_text_row(simple_query(s, "SELECT count(*) FROM wire_trigger_side WHERE id = 2")) == "0",
    )
    joined = simple_query(
        s,
        "CREATE TABLE wire_trigger_join_target (id integer PRIMARY KEY, value integer); "
        "CREATE TABLE wire_trigger_join_source (id integer, delta integer); "
        "CREATE TABLE wire_trigger_join_driver (id integer PRIMARY KEY, value integer); "
        "INSERT INTO wire_trigger_join_target VALUES (1, 10), (2, 20); "
        "INSERT INTO wire_trigger_join_source VALUES (1, 2), (2, 3); "
        "INSERT INTO wire_trigger_join_driver VALUES (1, 5); "
        "CREATE FUNCTION wire_trigger_join_fn() RETURNS trigger LANGUAGE plpgsql AS "
        "'BEGIN UPDATE wire_trigger_join_target "
        "SET value = wire_trigger_join_target.value + wire_trigger_join_source.delta + NEW.value - OLD.value "
        "FROM wire_trigger_join_source "
        "WHERE wire_trigger_join_target.id = wire_trigger_join_source.id AND OLD.id = NEW.id; "
        "DELETE FROM wire_trigger_join_target USING wire_trigger_join_source "
        "WHERE wire_trigger_join_target.id = wire_trigger_join_source.id "
        "AND wire_trigger_join_source.delta = 3 AND OLD.value = 5 AND NEW.value = 7; "
        "RETURN NEW; END'; "
        "CREATE TRIGGER wire_trigger_join AFTER UPDATE ON wire_trigger_join_driver "
        "FOR EACH ROW EXECUTE FUNCTION wire_trigger_join_fn(); "
        "UPDATE wire_trigger_join_driver SET value = 7 WHERE id = 1",
    )
    check("raw wire: joined trigger DML setup succeeds", not any(kind == b"E" for kind, _ in joined), joined)
    check(
        "raw wire: joined trigger DML preserves typed OLD/NEW",
        first_text_row(simple_query(s, "SELECT id || ':' || value FROM wire_trigger_join_target ORDER BY id"))
        == "1:14",
    )
    s.close()


def test_statement_and_conflict_triggers_over_raw_wire():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(
        s,
        "CREATE TABLE wire_statement_target (id integer PRIMARY KEY, value integer); "
        "CREATE TABLE wire_statement_audit (event text, value integer); "
        "CREATE FUNCTION wire_statement_note() RETURNS trigger LANGUAGE plpgsql AS "
        "'BEGIN INSERT INTO wire_statement_audit VALUES (''statement'', 0); RETURN NULL; END'; "
        "CREATE FUNCTION wire_conflict_note() RETURNS trigger LANGUAGE plpgsql AS "
        "'BEGIN INSERT INTO wire_statement_audit VALUES (''update'', NEW.value); RETURN NEW; END'; "
        "CREATE TRIGGER wire_statement_insert BEFORE INSERT ON wire_statement_target "
        "FOR EACH STATEMENT EXECUTE FUNCTION wire_statement_note(); "
        "CREATE TRIGGER wire_conflict_update BEFORE UPDATE OF value ON wire_statement_target "
        "FOR EACH ROW WHEN (NEW.value > OLD.value) EXECUTE FUNCTION wire_conflict_note(); "
        "INSERT INTO wire_statement_target VALUES (1, 1), (2, 2); "
        "INSERT INTO wire_statement_target VALUES (1, 5) "
        "ON CONFLICT (id) DO UPDATE SET value = excluded.value",
    )
    check("raw wire: statement and conflict trigger setup succeeds", not any(kind == b"E" for kind, _ in setup), setup)
    check(
        "raw wire: statement trigger fires once and conflict update fires",
        first_text_row(simple_query(s, "SELECT count(*) FROM wire_statement_audit")) == "3",
    )
    check(
        "raw wire: conflict row trigger changes the durable target",
        first_text_row(simple_query(s, "SELECT value FROM wire_statement_target WHERE id = 1")) == "5",
    )
    s.close()


def test_subscription_definition_lifecycle_over_raw_wire():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    created = simple_query(
        s,
        "CREATE SUBSCRIPTION wire_subscription CONNECTION 'host=publisher port=5432' "
        "PUBLICATION sales WITH (connect = false, slot_name = NONE)",
    )
    check(
        "raw wire: CREATE disabled subscription completes",
        any(kind == b"C" and payload == b"CREATE SUBSCRIPTION\x00" for kind, payload in created),
        created,
    )
    altered = simple_query(
        s,
        "ALTER SUBSCRIPTION wire_subscription CONNECTION 'host=publisher-two port=5433'; "
        "ALTER SUBSCRIPTION wire_subscription SET PUBLICATION inventory, sales WITH (refresh = false)",
    )
    check(
        "raw wire: ALTER subscription definition completes twice",
        sum(kind == b"C" and payload == b"ALTER SUBSCRIPTION\x00" for kind, payload in altered) == 2,
        altered,
    )
    catalog = simple_query(
        s,
        "SELECT subconninfo || '|' || subpublications::text FROM pg_subscription "
        "WHERE subname = 'wire_subscription'",
    )
    check(
        "raw wire: ALTER subscription changes catalog definition",
        first_text_row(catalog) == "host=publisher-two port=5433|{inventory,sales}",
        catalog,
    )
    s.close()


def test_catalog_aware_binary_bind_parameters():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    simple_query(
        s,
        "CREATE TYPE wire_binary_state AS ENUM ('ready', 'blocked'); "
        "CREATE DOMAIN wire_binary_positive AS integer CHECK (VALUE > 0); "
        "CREATE DOMAIN wire_binary_vector AS integer[]; "
        "CREATE DOMAIN wire_binary_required AS integer NOT NULL; "
        "CREATE TABLE wire_binary_regclass (id integer); "
        "CREATE FUNCTION wire_binary_routine(value integer) RETURNS integer LANGUAGE SQL "
        "AS 'SELECT value'",
    )

    enum_oid = int(first_text_row(simple_query(s, "SELECT oid FROM pg_type WHERE typname = 'wire_binary_state'")))
    domain_oid = int(first_text_row(simple_query(s, "SELECT oid FROM pg_type WHERE typname = 'wire_binary_positive'")))
    enum_array_oid = 160000 + enum_oid - 120000
    domain_array_oid = 150000 + domain_oid - 110000
    vector_oid = int(first_text_row(simple_query(s, "SELECT oid FROM pg_type WHERE typname = 'wire_binary_vector'")))
    vector_array_oid = 150000 + vector_oid - 110000
    required_domain_oid = int(
        first_text_row(simple_query(s, "SELECT oid FROM pg_type WHERE typname = 'wire_binary_required'"))
    )
    regclass_oid = int(
        first_text_row(simple_query(s, "SELECT oid FROM pg_class WHERE relname = 'wire_binary_regclass'"))
    )
    routine_oid = int(
        first_text_row(simple_query(s, "SELECT oid FROM pg_proc WHERE proname = 'wire_binary_routine'"))
    )
    cases = [
        ("unknown", "SELECT $1::text", 705, b"wire text", "wire text", None),
        ("regtype", "SELECT $1::regtype", 2206, struct.pack("!i", 23), "integer", None),
        (
            "regclass",
            "SELECT $1::regclass",
            2205,
            struct.pack("!i", regclass_oid),
            "wire_binary_regclass",
            None,
        ),
        ("regproc", "SELECT $1::regproc", 24, struct.pack("!i", routine_oid), "wire_binary_routine", None),
        (
            "regprocedure",
            "SELECT $1::regprocedure",
            2202,
            struct.pack("!i", routine_oid),
            "wire_binary_routine(integer)",
            None,
        ),
        (
            "regoperator",
            "SELECT $1::regoperator",
            2204,
            struct.pack("!i", 551),
            "+(integer,integer)",
            None,
        ),
        ("invalid regtype", "SELECT $1::regtype", 2206, b"\x00", None, "22P03"),
        ("json", "SELECT $1::json", 114, b'{"b": 1, "a": 2}', '{"b": 1, "a": 2}', None),
        ("jsonb", "SELECT $1::jsonb", 3802, b'\x01{"b": 1, "a": 2}', '{"a": 2, "b": 1}', None),
        ("enum", "SELECT $1::wire_binary_state", enum_oid, b"ready", "ready", None),
        ("domain", "SELECT $1::wire_binary_positive", domain_oid, struct.pack("!i", 7), "7", None),
        (
            "enum array",
            "SELECT $1::wire_binary_state[]",
            enum_array_oid,
            binary_array(enum_oid, [b"ready", b"blocked"]),
            "{ready,blocked}",
            None,
        ),
        (
            "domain array",
            "SELECT $1::wire_binary_positive[]",
            domain_array_oid,
            binary_array(domain_oid, [struct.pack("!i", 3), struct.pack("!i", 5)]),
            "{3,5}",
            None,
        ),
        (
            "array-valued domain array",
            "SELECT $1::wire_binary_vector[]",
            vector_array_oid,
            binary_array(vector_oid, [binary_array(23, [struct.pack("!i", 3), struct.pack("!i", 4)])]),
            '{"{3,4}"}',
            None,
        ),
        ("invalid enum", "SELECT $1::wire_binary_state", enum_oid, b"missing", None, "22P02"),
        ("invalid json", "SELECT $1::json", 114, b"{not json}", None, "22P02"),
        ("invalid jsonb", "SELECT $1::jsonb", 3802, b"\x01{not json}", None, "22P02"),
        (
            "invalid domain",
            "SELECT $1::wire_binary_positive",
            domain_oid,
            struct.pack("!i", -1),
            None,
            "23514",
        ),
        (
            "invalid domain array",
            "SELECT $1::wire_binary_positive[]",
            domain_array_oid,
            binary_array(domain_oid, [struct.pack("!i", -1)]),
            None,
            "23514",
        ),
        ("null required domain", "SELECT $1::wire_binary_required", required_domain_oid, None, None, "23502"),
    ]
    for name, text, oid, value, expected, state in cases:
        messages = extended_binary_parameter(s, text, oid, value)
        if expected is not None:
            check(
                f"binary Bind catalog {name}",
                first_text_row(messages) == expected,
                messages,
            )
        else:
            check(
                f"binary Bind catalog {name} rejects invalid value",
                has_sqlstate(messages, state),
                messages,
            )
    # A malformed composite body belongs to the binary input value, not the
    # surrounding COPY format. PostgreSQL exposes that distinction as 22P03.
    malformed_array = struct.pack("!iii", 1, 1, 23) + struct.pack("!ii", 1, 1) + struct.pack("!i", -2)
    messages = extended_binary_parameter(s, "SELECT $1::int4[]", 1007, malformed_array)
    check(
        "binary Bind malformed structured value has binary-input SQLSTATE",
        has_sqlstate(messages, "22P03"),
        messages,
    )
    record = binary_record(
        [
            (enum_oid, b"ready"),
            (domain_oid, struct.pack("!i", 7)),
            (domain_array_oid, binary_array(domain_oid, [struct.pack("!i", 3), struct.pack("!i", 5)])),
            (705, b"wire text"),
        ]
    )
    messages = extended_binary_parameter(s, "SELECT $1::record", 2249, record)
    check(
        "binary Bind record resolves nested catalog field types",
        first_text_row(messages) == '(ready,7,"{3,5}","wire text")',
        messages,
    )
    invalid_record = binary_record(
        [
            (enum_oid, b"ready"),
            (domain_oid, struct.pack("!i", -1)),
        ]
    )
    messages = extended_binary_parameter(s, "SELECT $1::record", 2249, invalid_record)
    check(
        "binary Bind record enforces nested domain constraints",
        has_sqlstate(messages, "23514"),
        messages,
    )
    simple_query(
        s,
        "CREATE TABLE wire_routine_values (id integer, value integer); "
        "INSERT INTO wire_routine_values VALUES (1, 40), (2, 41); "
        "CREATE FUNCTION wire_values_from(integer) RETURNS SETOF integer LANGUAGE SQL "
        "AS 'SELECT value FROM wire_routine_values WHERE id >= $1'",
    )
    messages = extended_binary_parameter(
        s,
        "SELECT value FROM wire_values_from($1) AS values_from(value) ORDER BY value",
        23,
        struct.pack("!i", 1),
    )
    check(
        "binary Bind resolves a set-returning SQL function parameter",
        [first_text_row([message]) for message in messages if message[0] == b"D"] == ["40", "41"],
        messages,
    )
    s.close()


def test_catalog_aware_text_bind_parameters():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    simple_query(
        s,
        "CREATE TYPE wire_text_state AS ENUM ('ready', 'blocked'); "
        "CREATE DOMAIN wire_text_positive AS integer CHECK (VALUE > 0); "
        "CREATE DOMAIN wire_text_required AS integer NOT NULL; "
        "CREATE TABLE wire_text_regclass (id integer); "
        "CREATE FUNCTION wire_text_routine(value integer) RETURNS integer LANGUAGE SQL "
        "AS 'SELECT value'",
    )
    enum_oid = int(first_text_row(simple_query(s, "SELECT oid FROM pg_type WHERE typname = 'wire_text_state'")))
    domain_oid = int(first_text_row(simple_query(s, "SELECT oid FROM pg_type WHERE typname = 'wire_text_positive'")))
    enum_array_oid = 160000 + enum_oid - 120000
    domain_array_oid = 150000 + domain_oid - 110000
    required_domain_oid = int(
        first_text_row(simple_query(s, "SELECT oid FROM pg_type WHERE typname = 'wire_text_required'"))
    )
    cases = [
        ("integer identity", "SELECT pg_typeof($1)", 23, b"7", "integer", None),
        ("unknown", "SELECT $1::text", 705, b"wire text", "wire text", None),
        ("regtype", "SELECT $1::regtype", 2206, b"integer", "integer", None),
        ("regclass", "SELECT $1::regclass", 2205, b"wire_text_regclass", "wire_text_regclass", None),
        ("regproc", "SELECT $1::regproc", 24, b"wire_text_routine", "wire_text_routine", None),
        (
            "regprocedure",
            "SELECT $1::regprocedure",
            2202,
            b"wire_text_routine(integer)",
            "wire_text_routine(integer)",
            None,
        ),
        (
            "regoperator",
            "SELECT $1::regoperator",
            2204,
            b"+(integer,integer)",
            "+(integer,integer)",
            None,
        ),
        ("invalid regtype", "SELECT $1::regtype", 2206, b"not_a_type", None, "42704"),
        ("enum", "SELECT $1::wire_text_state", enum_oid, b"ready", "ready", None),
        ("domain", "SELECT $1::wire_text_positive", domain_oid, b"7", "7", None),
        ("enum array", "SELECT $1::wire_text_state[]", enum_array_oid, b"{ready,blocked}", "{ready,blocked}", None),
        ("domain array", "SELECT $1::wire_text_positive[]", domain_array_oid, b"{3,5}", "{3,5}", None),
        ("invalid UTF-8", "SELECT $1::text", 25, b"\xff", None, "22021"),
        ("invalid enum", "SELECT $1::wire_text_state", enum_oid, b"missing", None, "22P02"),
        ("invalid domain", "SELECT $1::wire_text_positive", domain_oid, b"-1", None, "23514"),
        ("invalid domain array", "SELECT $1::wire_text_positive[]", domain_array_oid, b"{3,-1}", None, "23514"),
        ("null required domain", "SELECT $1::wire_text_required", required_domain_oid, None, None, "23502"),
    ]
    for name, text, oid, value, expected, state in cases:
        messages = extended_text_parameter(s, text, oid, value)
        if expected is not None:
            check(f"text Bind catalog {name}", first_text_row(messages) == expected, messages)
        else:
            check(
                f"text Bind catalog {name} rejects invalid value",
                has_sqlstate(messages, state),
                messages,
            )
    s.close()


def test_transition_tables_over_raw_simple_query():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(
        s,
        "CREATE TABLE wire_transition_target (id integer PRIMARY KEY, value integer); "
        "CREATE TABLE wire_transition_audit (id integer, value integer); "
        "CREATE FUNCTION wire_transition_rows() RETURNS trigger LANGUAGE plpgsql AS "
        "'BEGIN INSERT INTO wire_transition_audit SELECT id, value FROM inserted_rows; RETURN NULL; END'; "
        "CREATE TRIGGER wire_transition_insert AFTER INSERT ON wire_transition_target "
        "REFERENCING NEW TABLE AS inserted_rows FOR EACH STATEMENT "
        "EXECUTE FUNCTION wire_transition_rows(); "
        "INSERT INTO wire_transition_target VALUES (1, 10), (2, 20)",
    )
    check("raw Query creates and executes a transition-table trigger", not any(m[0] == b"E" for m in setup), setup)
    rows = [
        text_row_fields(payload)
        for kind, payload in simple_query(s, "SELECT id, value FROM wire_transition_audit ORDER BY id")
        if kind == b"D"
    ]
    check("raw Query transition relation rows", rows == [["1", "10"], ["2", "20"]], rows)
    s.close()


def test_typed_trigger_query_program_over_raw_simple_query():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(
        s,
        "CREATE TABLE wire_local_target (id integer PRIMARY KEY, value integer); "
        "CREATE TABLE wire_local_source (id integer PRIMARY KEY, delta integer); "
        "CREATE TABLE wire_local_audit (value integer); "
        "INSERT INTO wire_local_target VALUES (1, 5); "
        "INSERT INTO wire_local_source VALUES (1, 3); "
        "CREATE FUNCTION wire_local_program() RETURNS trigger LANGUAGE plpgsql AS "
        "'DECLARE change integer := NEW.value - OLD.value; selected_delta integer; "
        "BEGIN SELECT source.delta INTO selected_delta FROM wire_local_source source "
        "WHERE source.id = NEW.id; selected_delta := selected_delta + 1; "
        "PERFORM 1 FROM wire_local_source source WHERE source.id = NEW.id "
        "AND selected_delta = source.delta + 1; "
        "INSERT INTO wire_local_audit VALUES (selected_delta); "
        "NEW.value := NEW.value + change + selected_delta; RETURN NEW; END'; "
        "CREATE TRIGGER wire_local_before BEFORE UPDATE ON wire_local_target "
        "FOR EACH ROW EXECUTE FUNCTION wire_local_program(); "
        "UPDATE wire_local_target SET value = 7 WHERE id = 1",
    )
    check("raw wire: typed trigger query program setup succeeds", not any(kind == b"E" for kind, _ in setup), setup)
    check(
        "raw wire: typed trigger query program updates NEW and local audit",
        first_text_row(simple_query(s, "SELECT value FROM wire_local_target")) == "13"
        and first_text_row(simple_query(s, "SELECT value FROM wire_local_audit")) == "4",
    )
    s.close()


def main():
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_")]
    for t in tests:
        print(t.__name__)
        t()
    if failures:
        print(f"\n{len(failures)} wire probe(s) FAILED: {failures}")
        sys.exit(1)
    print("\nall wire probes passed")


if __name__ == "__main__":
    main()

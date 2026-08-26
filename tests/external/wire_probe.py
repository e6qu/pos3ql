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


def test_record_fields_preserve_type_modifiers():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    query = (
        "WITH source AS (SELECT ROW('abc'::varchar(5), 'label'::text COLLATE \"C\") AS q) "
        "SELECT (q).f1, (q).f2 FROM source"
    )
    parse = frontend_message(b"P", b"wire_record_meta_statement\x00" + query.encode() + b"\x00\x00\x00")
    bind = frontend_message(
        b"B",
        b"wire_record_meta_portal\x00wire_record_meta_statement\x00"
        + struct.pack("!hhhhh", 0, 0, 2, 1, 1),
    )
    describe = frontend_message(b"D", b"Pwire_record_meta_portal\x00")
    s.sendall(parse + bind + describe + frontend_message(b"S"))
    out = []
    while True:
        item = read_message(s)
        out.append(item)
        if item[0] == b"Z":
            break
    description = next((payload for kind, payload in out if kind == b"T"), None)
    check(
        "record fields retain OIDs, typmods, and binary formats",
        description is not None
        and row_description_type_oids(description) == [1043, 25]
        and row_description_type_modifiers(description) == [9, -1]
        and row_description_formats(description) == [1, 1],
        out,
    )
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


def test_fetch_honors_mixed_result_formats_independently_of_cursor_declaration():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    declared = simple_query(
        s,
        "BEGIN; DECLARE mixed_cursor CURSOR FOR "
        "SELECT 123.45::numeric(7,2) AS amount, 'abc'::text AS label",
    )
    check(
        "SQL cursor: ordinary declaration succeeds",
        not any(kind == b"E" for kind, _ in declared),
        declared,
    )
    parse = frontend_message(
        b"P",
        b"mixed_fetch_statement\x00FETCH ALL FROM mixed_cursor\x00\x00\x00",
    )
    bind = frontend_message(
        b"B",
        b"mixed_fetch_portal\x00mixed_fetch_statement\x00"
        + struct.pack("!hhhhh", 0, 0, 2, 1, 0),
    )
    describe = frontend_message(b"D", b"Pmixed_fetch_portal\x00")
    execute = frontend_message(b"E", b"mixed_fetch_portal\x00\x00\x00\x00\x00")
    s.sendall(parse + bind + describe + execute + frontend_message(b"S"))
    out = []
    while True:
        item = read_message(s)
        out.append(item)
        if item[0] == b"Z":
            break
    numeric = struct.pack("!hhhhhh", 2, 0, 0, 2, 123, 4500)
    expected = b"\x00\x02" + struct.pack("!i", len(numeric)) + numeric
    expected += struct.pack("!i", 3) + b"abc"
    row = next((payload for kind, payload in out if kind == b"D"), None)
    description = next((payload for kind, payload in out if kind == b"T"), None)
    check(
        "SQL cursor: FETCH chooses binary numeric and text independently",
        description is not None
        and row_description_formats(description) == [1, 0]
        and row == expected
        and not any(kind == b"E" for kind, _ in out),
        out,
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


def test_binary_portal_paging_retains_result_shape_and_format():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(
        s,
        "CREATE TABLE wire_paged_portal (id integer PRIMARY KEY, value varchar(3)); "
        "INSERT INTO wire_paged_portal VALUES (1, 'one'), (2, 'two'), (3, 'tri')",
    )
    check("binary portal paging setup", not any(kind == b"E" for kind, _ in setup), setup)
    transaction = simple_query(s, "BEGIN")
    check("binary portal paging transaction", not any(kind == b"E" for kind, _ in transaction), transaction)
    parse = frontend_message(
        b"P",
        b"paged_binary_statement\x00SELECT (wire_paged_portal).* FROM wire_paged_portal ORDER BY id\x00\x00\x00",
    )
    bind = frontend_message(
        b"B",
        b"paged_binary_portal\x00paged_binary_statement\x00" + struct.pack("!hhhhh", 0, 0, 2, 1, 1),
    )
    describe = frontend_message(b"D", b"Ppaged_binary_portal\x00")
    s.sendall(parse + bind + describe + frontend_message(b"S"))
    describe_out = []
    while True:
        item = read_message(s)
        describe_out.append(item)
        if item[0] == b"Z":
            break
    description = next((payload for kind, payload in describe_out if kind == b"T"), None)
    check(
        "binary paged portal Describe retains formats and varchar typmod",
        description is not None
        and row_description_type_oids(description) == [23, 1043]
        and row_description_type_modifiers(description) == [-1, 7]
        and row_description_formats(description) == [1, 1],
        describe_out,
    )

    expected_rows = [
        b"\x00\x02\x00\x00\x00\x04\x00\x00\x00\x01\x00\x00\x00\x03one",
        b"\x00\x02\x00\x00\x00\x04\x00\x00\x00\x02\x00\x00\x00\x03two",
        b"\x00\x02\x00\x00\x00\x04\x00\x00\x00\x03\x00\x00\x00\x03tri",
    ]
    for index, expected in enumerate(expected_rows):
        execute = frontend_message(b"E", b"paged_binary_portal\x00\x00\x00\x00\x01")
        s.sendall(execute + frontend_message(b"S"))
        out = []
        while True:
            item = read_message(s)
            out.append(item)
            if item[0] == b"Z":
                break
        row = next((payload for kind, payload in out if kind == b"D"), None)
        check(
            f"binary paged portal Execute {index + 1} retains row bytes and suspension",
            row == expected and [kind for kind, _ in out] == [b"D", b"s", b"Z"],
            out,
        )
    s.sendall(frontend_message(b"E", b"paged_binary_portal\x00\x00\x00\x00\x01") + frontend_message(b"S"))
    completed = []
    while True:
        item = read_message(s)
        completed.append(item)
        if item[0] == b"Z":
            break
    check(
        "binary paged portal detects exhaustion on the following Execute",
        [kind for kind, _ in completed] == [b"C", b"Z"]
        and completed[0][1] == b"SELECT 0\x00",
        completed,
    )
    s.close()


def test_portal_lifetime_and_execute_counts_match_postgresql():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)

    parse = frontend_message(
        b"P", b"idle_statement\x00SELECT generate_series(1,2)\x00\x00\x00"
    )
    bind = frontend_message(
        b"B", b"idle_portal\x00idle_statement\x00" + struct.pack("!hhh", 0, 0, 0)
    )
    s.sendall(parse + bind + frontend_message(b"S"))
    while read_message(s)[0] != b"Z":
        pass
    s.sendall(
        frontend_message(b"E", b"idle_portal\x00\x00\x00\x00\x01")
        + frontend_message(b"S")
    )
    expired = []
    while True:
        item = read_message(s)
        expired.append(item)
        if item[0] == b"Z":
            break
    error = next((payload for kind, payload in expired if kind == b"E"), b"")
    check(
        "portal lifetime: Sync destroys an idle transaction's portal",
        b"C34000\x00" in error,
        expired,
    )

    parse = frontend_message(
        b"P", b"paged_statement\x00SELECT generate_series(1,2)\x00\x00\x00"
    )
    bind = frontend_message(
        b"B", b"paged_portal\x00paged_statement\x00" + struct.pack("!hhh", 0, 0, 0)
    )
    execute = frontend_message(b"E", b"paged_portal\x00\x00\x00\x00\x01")
    s.sendall(parse + bind + execute + execute + execute + execute + frontend_message(b"S"))
    paged = []
    while True:
        item = read_message(s)
        paged.append(item)
        if item[0] == b"Z":
            break
    check(
        "portal paging: exact pages suspend, exhaustion and repeats report SELECT 0",
        [kind for kind, _ in paged]
        == [b"1", b"2", b"D", b"s", b"D", b"s", b"C", b"C", b"Z"]
        and [payload for kind, payload in paged if kind == b"C"]
        == [b"SELECT 0\x00", b"SELECT 0\x00"],
        paged,
    )
    s.close()


def test_paged_portal_capacity_is_a_named_error_not_a_disconnect():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    simple_query(s, "BEGIN")
    parse = frontend_message(
        b"P",
        b"bounded_statement\x00SELECT repeat('x', 1024) FROM generate_series(1,100)\x00\x00\x00",
    )
    bind = frontend_message(
        b"B", b"bounded_portal\x00bounded_statement\x00" + struct.pack("!hhh", 0, 0, 0)
    )
    execute = frontend_message(b"E", b"bounded_portal\x00\x00\x00\x00\x01")
    s.sendall(parse + bind + execute + frontend_message(b"S"))
    output = []
    while True:
        item = read_message(s)
        output.append(item)
        if item[0] == b"Z":
            break
    error = next((payload for kind, payload in output if kind == b"E"), b"")
    check(
        "bounded portal: result capacity reports 54000 and keeps the connection",
        b"C54000\x00" in error
        and b"statement response exceeds its configured buffer" in error
        and output[-1][0] == b"Z",
        output,
    )
    s.close()


def test_named_portals_cannot_be_silently_rebound():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    simple_query(s, "BEGIN")
    parse = frontend_message(b"P", b"duplicate_portal_statement\x00SELECT 7\x00\x00\x00")
    bind = frontend_message(
        b"B",
        b"duplicate_portal\x00duplicate_portal_statement\x00" + struct.pack("!hhh", 0, 0, 0),
    )
    s.sendall(parse + bind + bind + frontend_message(b"S"))
    duplicate = []
    while True:
        item = read_message(s)
        duplicate.append(item)
        if item[0] == b"Z":
            break
    check(
        "named portal: duplicate Bind is 42P03",
        has_sqlstate(duplicate, "42P03") and duplicate[-1] == (b"Z", b"T"),
        duplicate,
    )
    execute = frontend_message(b"E", b"duplicate_portal\x00\x00\x00\x00\x00")
    s.sendall(execute + frontend_message(b"S"))
    original = []
    while True:
        item = read_message(s)
        original.append(item)
        if item[0] == b"Z":
            break
    check(
        "named portal: failed rebind retains the original portal",
        first_text_row(original) == "7" and not any(kind == b"E" for kind, _ in original),
        original,
    )
    s.close()


def test_rows_from_binary_portal_retains_lockstep_shape_and_format():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    transaction = simple_query(s, "BEGIN")
    check("ROWS FROM portal paging transaction", not any(kind == b"E" for kind, _ in transaction), transaction)
    query = (
        "SELECT * FROM ROWS FROM (generate_series(1,2), "
        "unnest(ARRAY['x']::varchar(2)[])) WITH ORDINALITY "
        "AS r(series,label,ordinality) ORDER BY ordinality"
    )
    parse = frontend_message(
        b"P", b"rows_from_statement\x00" + query.encode() + b"\x00\x00\x00"
    )
    bind = frontend_message(
        b"B",
        b"rows_from_portal\x00rows_from_statement\x00"
        + struct.pack("!hhhhhh", 0, 0, 3, 1, 1, 1),
    )
    describe = frontend_message(b"D", b"Prows_from_portal\x00")
    s.sendall(parse + bind + describe + frontend_message(b"S"))
    described = []
    while True:
        item = read_message(s)
        described.append(item)
        if item[0] == b"Z":
            break
    description = next((payload for kind, payload in described if kind == b"T"), None)
    check(
        "ROWS FROM portal Describe retains OIDs, typmod, and binary formats",
        description is not None
        and row_description_type_oids(description) == [23, 1043, 20]
        and row_description_type_modifiers(description) == [-1, 6, -1]
        and row_description_formats(description) == [1, 1, 1],
        described,
    )
    expected_rows = [
        b"\x00\x03\x00\x00\x00\x04\x00\x00\x00\x01\x00\x00\x00\x01x"
        b"\x00\x00\x00\x08\x00\x00\x00\x00\x00\x00\x00\x01",
        b"\x00\x03\x00\x00\x00\x04\x00\x00\x00\x02\xff\xff\xff\xff"
        b"\x00\x00\x00\x08\x00\x00\x00\x00\x00\x00\x00\x02",
    ]
    for index, expected in enumerate(expected_rows):
        execute = frontend_message(b"E", b"rows_from_portal\x00\x00\x00\x00\x01")
        s.sendall(execute + frontend_message(b"S"))
        output = []
        while True:
            item = read_message(s)
            output.append(item)
            if item[0] == b"Z":
                break
        row = next((payload for kind, payload in output if kind == b"D"), None)
        check(
            f"ROWS FROM binary portal page {index + 1} preserves NULL padding",
            row == expected and [kind for kind, _ in output] == [b"D", b"s", b"Z"],
            output,
        )
    s.sendall(frontend_message(b"E", b"rows_from_portal\x00\x00\x00\x00\x01") + frontend_message(b"S"))
    completed = []
    while True:
        item = read_message(s)
        completed.append(item)
        if item[0] == b"Z":
            break
    check(
        "ROWS FROM portal detects exhaustion on the following Execute",
        [kind for kind, _ in completed] == [b"C", b"Z"]
        and completed[0][1] == b"SELECT 0\x00",
        completed,
    )
    s.close()


def test_catalog_srf_identity_and_domain_wire_boundary():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(
        s,
        "CREATE DOMAIN wire_srf_count AS integer CHECK (VALUE > 0); "
        "CREATE FUNCTION wire_srf_scalar(value wire_srf_count) RETURNS wire_srf_count "
        "LANGUAGE SQL AS 'SELECT $1'; "
        "CREATE FUNCTION wire_srf_record(value integer) "
        "RETURNS TABLE(number integer, source text) LANGUAGE SQL AS 'SELECT 1, ''integer'''; "
        "CREATE FUNCTION wire_srf_record(value wire_srf_count) "
        "RETURNS TABLE(label text, accepted boolean) LANGUAGE SQL AS 'SELECT ''domain'', true'; "
        "CREATE FUNCTION wire_srf_one(value wire_srf_count) "
        "RETURNS TABLE(label text) LANGUAGE SQL AS 'SELECT ''domain'''; "
        "CREATE TABLE wire_srf_input(value wire_srf_count); "
        "INSERT INTO wire_srf_input VALUES (1)",
    )
    check("catalog SRF raw-wire setup", not any(kind == b"E" for kind, _ in setup), setup)

    query = (
        "SELECT wire_srf_scalar(input.value), (wire_srf_record(input.value)).* "
        "FROM wire_srf_input AS input"
    )
    parse = frontend_message(
        b"P", b"wire_srf_statement\x00" + query.encode() + b"\x00\x00\x00"
    )
    bind = frontend_message(
        b"B",
        b"wire_srf_portal\x00wire_srf_statement\x00"
        + struct.pack("!hhhhhh", 0, 0, 3, 1, 1, 1),
    )
    describe = frontend_message(b"D", b"Pwire_srf_portal\x00")
    execute = frontend_message(b"E", b"wire_srf_portal\x00\x00\x00\x00\x00")
    s.sendall(parse + bind + describe + execute + frontend_message(b"S"))
    output = []
    while True:
        item = read_message(s)
        output.append(item)
        if item[0] == b"Z":
            break
    description = next((payload for kind, payload in output if kind == b"T"), None)
    row = next((payload for kind, payload in output if kind == b"D"), None)
    expected = (
        b"\x00\x03\x00\x00\x00\x04\x00\x00\x00\x01"
        b"\x00\x00\x00\x06domain\x00\x00\x00\x01\x01"
    )
    check(
        "catalog SRF Describe separates domain identity from wire representation",
        description is not None
        and row_description_type_oids(description) == [23, 25, 16]
        and row_description_formats(description) == [1, 1, 1],
        output,
    )
    check(
        "catalog SRF binary Result uses the domain overload and record shape",
        row == expected,
        row,
    )

    scalar_star = simple_query(s, "SELECT (wire_srf_one(1::wire_srf_count)).*")
    check(
        "single-column RETURNS TABLE is not a record expression",
        any(kind == b"E" for kind, _ in scalar_star) and has_sqlstate(scalar_star, "42809"),
        scalar_star,
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

    def run(text, expected_oid, expected_width=None):
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
            and len(rows) == 3
            and (
                expected_width is None
                or all(
                    len(row) == 6 + expected_width
                    and struct.unpack("!i", row[2:6])[0] == expected_width
                    for row in rows
                )
            ),
            out,
        )

    run("SELECT generate_series(1, 3)", 23, 4)
    run("SELECT generate_series(1, 3::bigint)", 20, 8)
    run(
        "SELECT generate_series('2000-01-01'::timestamp, '2000-01-03'::timestamp, '1 day'::interval)",
        1114,
        8,
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
        "CREATE PUBLICATION wire_replication_pub FOR TABLE wire_replication WHERE (id > 0) "
        "WITH (publish = 'insert'); "
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
        "BEGIN; INSERT INTO wire_replication VALUES (-42); INSERT INTO wire_replication VALUES (42); "
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
        and not any(payload.endswith(b"-42") for payload in inserts)
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


def test_extension_lifecycle_over_raw_wire():
    if os.environ.get("POS3QL_EXTENSION_WIRE") != "1":
        return
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(
        s,
        "CREATE SCHEMA wire_extension; "
        "CREATE EXTENSION pos3ql_ext VERSION '1.0' SCHEMA wire_extension CASCADE; "
        "ALTER EXTENSION pos3ql_ext UPDATE TO '2.0'",
    )
    tags = [payload.rstrip(b"\x00") for kind, payload in setup if kind == b"C"]
    check(
        "raw wire: extension create and transactional update complete",
        tags == [b"CREATE SCHEMA", b"CREATE EXTENSION", b"ALTER EXTENSION"]
        and not any(kind == b"E" for kind, _ in setup),
        setup,
    )

    query = (
        "SELECT extname, extversion, extrelocatable FROM pg_extension "
        "WHERE extname::text = $1"
    )
    parse = frontend_message(
        b"P", b"wire_extension_statement\x00" + query.encode() + b"\x00" + struct.pack("!hi", 1, 25)
    )
    value = b"pos3ql_ext"
    bind = frontend_message(
        b"B",
        b"wire_extension_portal\x00wire_extension_statement\x00"
        + struct.pack("!hhh", 1, 0, 1)
        + struct.pack("!i", len(value))
        + value
        + struct.pack("!h", 0),
    )
    describe = frontend_message(b"D", b"Pwire_extension_portal\x00")
    execute = frontend_message(b"E", b"wire_extension_portal\x00\x00\x00\x00\x00")
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
        "raw wire: extension catalogs retain PostgreSQL types through a named portal",
        description is not None
        and row_description_type_oids(description) == [19, 25, 16]
        and row is not None
        and text_row_fields(row) == ["pos3ql_ext", "2.0", "t"],
        out,
    )

    cleanup = simple_query(
        s,
        "DROP EXTENSION pos3ql_ext; DROP EXTENSION pos3ql_base; "
        "DROP SCHEMA wire_extension",
    )
    check(
        "raw wire: extension drop removes durable members without CASCADE",
        not any(kind == b"E" for kind, _ in cleanup),
        cleanup,
    )
    s.close()


def binary_array(element_oid, values):
    return binary_array_shaped(element_oid, [len(values)], [1], values)


def binary_array_shaped(element_oid, dimensions, lower_bounds, values):
    if len(dimensions) != len(lower_bounds):
        raise ValueError("array dimensions and lower bounds must align")
    count = 1
    for dimension in dimensions:
        count *= dimension
    if count != len(values):
        raise ValueError("array values do not match dimensions")
    body = struct.pack(
        "!iii", len(dimensions), int(any(value is None for value in values)), element_oid
    )
    for dimension, lower_bound in zip(dimensions, lower_bounds):
        body += struct.pack("!ii", dimension, lower_bound)
    for value in values:
        if value is None:
            body += struct.pack("!i", -1)
        else:
            body += struct.pack("!i", len(value)) + value
    return body


def binary_bit(bits):
    packed = bytearray((len(bits) + 7) // 8)
    for index, bit in enumerate(bits):
        if bit == "1":
            packed[index // 8] |= 0x80 >> (index % 8)
    return struct.pack("!i", len(bits)) + bytes(packed)


def binary_record(fields):
    body = struct.pack("!i", len(fields))
    for field_oid, value in fields:
        body += struct.pack("!ii", field_oid, -1 if value is None else len(value))
        if value is not None:
            body += value
    return body


def binary_int4_range(lower, upper):
    return (
        b"\x02"
        + struct.pack("!i", 4)
        + struct.pack("!i", lower)
        + struct.pack("!i", 4)
        + struct.pack("!i", upper)
    )


def binary_multirange(ranges):
    return struct.pack("!i", len(ranges)) + b"".join(
        struct.pack("!i", len(item)) + item for item in ranges
    )


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


def extended_binary_result(s, text):
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


def test_catalog_definition_oid_over_raw_wire():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(
        s,
        "CREATE TABLE wire_catalog_definition (id integer PRIMARY KEY, value text); "
        "CREATE INDEX wire_catalog_definition_value_idx ON wire_catalog_definition "
        "(value COLLATE \"C\" DESC NULLS LAST) INCLUDE (id) "
        "WITH (fillfactor=80, deduplicate_items=off) WHERE id > 0",
    )
    check("raw wire: catalog definition setup succeeds", not any(kind == b"E" for kind, _ in setup), setup)
    index_oid = int(
        first_text_row(
            simple_query(
                s,
                "SELECT c.oid FROM pg_class c WHERE c.relname = 'wire_catalog_definition_value_idx'",
            )
        )
    )

    def query_oid(oid):
        parse = frontend_message(
            b"P", b"\x00SELECT pg_get_indexdef($1)\x00" + struct.pack("!hi", 1, 26)
        )
        bind = frontend_message(
            b"B",
            b"\x00\x00" + struct.pack("!hhh", 1, 1, 1) + struct.pack("!i", 4) + struct.pack("!I", oid) + struct.pack("!h", 0),
        )
        s.sendall(parse + bind + frontend_message(b"E", b"\x00\x00\x00\x00\x00") + frontend_message(b"S"))
        out = []
        while True:
            item = read_message(s)
            out.append(item)
            if item[0] == b"Z":
                return out

    definition = query_oid(index_oid)
    check(
        "raw wire: binary oid reaches executable pg_get_indexdef",
        first_text_row(definition)
        == "CREATE INDEX wire_catalog_definition_value_idx ON public.wire_catalog_definition USING btree "
        "(value COLLATE \"C\" DESC NULLS LAST) INCLUDE (id) "
        "WITH (fillfactor='80', deduplicate_items=off) WHERE id > 0",
        definition,
    )
    overflow = query_oid(0xFFFFFFFF)
    check("raw wire: unsigned OID overflow is loud", has_sqlstate(overflow, "22003"), overflow)
    s.close()


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


def test_trigger_function_replacement_over_raw_wire():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(
        s,
        "CREATE TABLE wire_replacement_target (id integer PRIMARY KEY); "
        "CREATE FUNCTION wire_replacement_gate() RETURNS trigger LANGUAGE plpgsql AS "
        "'BEGIN RETURN NEW; END'; "
        "CREATE TRIGGER wire_replacement_gate BEFORE INSERT ON wire_replacement_target "
        "FOR EACH ROW EXECUTE FUNCTION wire_replacement_gate()",
    )
    check("raw wire: replacement trigger setup succeeds", not any(kind == b"E" for kind, _ in setup), setup)
    before = first_text_row(simple_query(s, "SELECT oid FROM pg_proc WHERE proname = 'wire_replacement_gate'"))
    after = first_text_row(
        simple_query(
            s,
            "CREATE OR REPLACE FUNCTION wire_replacement_gate() RETURNS trigger LANGUAGE plpgsql AS "
            "'BEGIN RETURN NULL; END'; "
            "INSERT INTO wire_replacement_target VALUES (1); "
            "SELECT oid || ':' || (SELECT count(*) FROM wire_replacement_target) "
            "FROM pg_proc WHERE proname = 'wire_replacement_gate'",
        )
    )
    check(
        "raw wire: replacing a trigger function preserves OID and dependency",
        before is not None and after == f"{before}:0",
        (before, after),
    )
    s.close()


def test_instead_of_view_trigger_over_raw_wire():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(
        s,
        "CREATE TABLE wire_view_base (id integer PRIMARY KEY, value integer); "
        "INSERT INTO wire_view_base VALUES (1, 10), (2, 20); "
        "CREATE VIEW wire_view AS SELECT id, value FROM wire_view_base; "
        "CREATE FUNCTION wire_view_write() RETURNS trigger LANGUAGE plpgsql AS "
        "'BEGIN IF TG_OP = ''INSERT'' THEN INSERT INTO wire_view_base VALUES (NEW.id, NEW.value); RETURN NEW; "
        "ELSIF TG_OP = ''UPDATE'' THEN UPDATE wire_view_base SET value = NEW.value WHERE id = OLD.id; RETURN NEW; "
        "END IF; DELETE FROM wire_view_base WHERE id = OLD.id; RETURN OLD; END'; "
        "CREATE TRIGGER wire_view_write INSTEAD OF INSERT OR UPDATE OR DELETE ON wire_view "
        "FOR EACH ROW EXECUTE FUNCTION wire_view_write(); "
        "INSERT INTO wire_view (value, id) SELECT value, id FROM (VALUES (30, 3)) supplied(value, id); "
        "UPDATE wire_view SET value = 21 WHERE id = 2; "
        "DELETE FROM wire_view WHERE id = 1; "
        "CREATE TABLE wire_view_source (id integer PRIMARY KEY, value integer); "
        "INSERT INTO wire_view_source VALUES (2, 200), (3, 300); "
        "UPDATE wire_view AS target SET value = source.value FROM wire_view_source AS source "
        "WHERE target.id = source.id; "
        "DELETE FROM wire_view AS target USING wire_view_source AS source "
        "WHERE target.id = source.id AND source.id = 2",
    )
    check("raw wire: INSTEAD OF view setup and DML complete", not any(kind == b"E" for kind, _ in setup), setup)
    check(
        "raw wire: INSTEAD OF view trigger changes only its base-table actions",
        first_text_row(simple_query(s, "SELECT string_agg(id::text || ':' || value::text, ',' ORDER BY id) FROM wire_view_base"))
        == "3:300",
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
        "CREATE TYPE wire_binary_coordinate AS (x integer, y integer); "
        "CREATE DOMAIN wire_binary_coordinate_domain AS wire_binary_coordinate; "
        "CREATE TABLE wire_binary_regclass (id integer, state wire_binary_state, positive wire_binary_positive, coordinate wire_binary_coordinate); "
        "INSERT INTO wire_binary_regclass VALUES (1, 'ready', 7, ROW(4,8)::wire_binary_coordinate); "
        "CREATE FUNCTION wire_binary_routine(value integer) RETURNS integer LANGUAGE SQL "
        "AS 'SELECT value'; "
        "CREATE FUNCTION wire_binary_state_echo(value wire_binary_state) RETURNS wire_binary_state LANGUAGE SQL AS 'SELECT $1'; "
        "CREATE FUNCTION wire_binary_positive_echo(value wire_binary_positive) RETURNS wire_binary_positive LANGUAGE SQL AS 'SELECT $1'; "
        "CREATE FUNCTION wire_binary_coordinate_echo(value wire_binary_coordinate) RETURNS wire_binary_coordinate LANGUAGE SQL AS 'SELECT $1'; "
        "CREATE FUNCTION wire_binary_state_array_echo(value wire_binary_state[]) RETURNS wire_binary_state[] LANGUAGE SQL AS 'SELECT $1'; "
        "CREATE FUNCTION wire_binary_positive_array_echo(value wire_binary_positive[]) RETURNS wire_binary_positive[] LANGUAGE SQL AS 'SELECT $1'; "
        "CREATE FUNCTION wire_binary_coordinate_array_echo(value wire_binary_coordinate[]) RETURNS wire_binary_coordinate[] LANGUAGE SQL AS 'SELECT $1'",
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
    required_domain_array_oid = 150000 + required_domain_oid - 110000
    coordinate_domain_oid = int(
        first_text_row(
            simple_query(s, "SELECT oid FROM pg_type WHERE typname = 'wire_binary_coordinate_domain'")
        )
    )
    coordinate_oid = int(
        first_text_row(simple_query(s, "SELECT oid FROM pg_type WHERE typname = 'wire_binary_coordinate'"))
    )
    coordinate_array_oid = 240000 + coordinate_oid - 230000
    coordinate_domain_array_oid = 150000 + coordinate_domain_oid - 110000
    coordinate = binary_record([(23, struct.pack("!i", 4)), (23, struct.pack("!i", 8))])
    typed_record = binary_record(
        [
            (enum_oid, b"ready"),
            (domain_oid, struct.pack("!i", 7)),
            (coordinate_oid, coordinate),
        ]
    )
    for name, query in [
        (
            "cast fields",
            "SELECT ROW('ready'::wire_binary_state, 7::wire_binary_positive, "
            "ROW(4,8)::wire_binary_coordinate)",
        ),
        ("declared column fields", "SELECT ROW(state, positive, coordinate) FROM wire_binary_regclass"),
        (
            "routine result fields",
            "SELECT ROW(wire_binary_state_echo('ready'::wire_binary_state), "
            "wire_binary_positive_echo(7::wire_binary_positive), "
            "wire_binary_coordinate_echo(ROW(4,8)::wire_binary_coordinate))",
        ),
    ]:
        messages = extended_binary_result(s, query)
        description = next((payload for kind, payload in messages if kind == b"T"), None)
        row = next((payload for kind, payload in messages if kind == b"D"), None)
        check(
            f"binary Result ROW preserves {name} identities",
            description is not None
            and row_description_type_oids(description) == [2249]
            and row_description_formats(description) == [1]
            and row == b"\x00\x01" + struct.pack("!i", len(typed_record)) + typed_record,
            messages,
        )
    regclass_oid = int(
        first_text_row(simple_query(s, "SELECT oid FROM pg_class WHERE relname = 'wire_binary_regclass'"))
    )
    routine_oid = int(
        first_text_row(simple_query(s, "SELECT oid FROM pg_proc WHERE proname = 'wire_binary_routine'"))
    )
    namespace_oid = int(
        first_text_row(
            simple_query(s, "SELECT oid FROM pg_namespace WHERE nspname = 'public'")
        )
    )
    role_name = first_text_row(simple_query(s, "SELECT current_user"))
    role_oid = int(first_text_row(simple_query(s, "SELECT oid FROM pg_roles WHERE rolname = current_user")))
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
        (
            "regtype array",
            "SELECT $1::regtype[]",
            2211,
            binary_array(2206, [struct.pack("!i", 23)]),
            "{integer}",
            None,
        ),
        (
            "regproc array",
            "SELECT $1::regproc[]",
            1008,
            binary_array(24, [struct.pack("!i", routine_oid)]),
            "{wire_binary_routine}",
            None,
        ),
        (
            "regprocedure array",
            "SELECT $1::regprocedure[]",
            2207,
            binary_array(2202, [struct.pack("!i", routine_oid)]),
            "{wire_binary_routine(integer)}",
            None,
        ),
        (
            "regoper array",
            "SELECT $1::regoper[]",
            2208,
            binary_array(2203, [struct.pack("!i", 551)]),
            "{pg_catalog.+}",
            None,
        ),
        (
            "regoperator array",
            "SELECT $1::regoperator[]",
            2209,
            binary_array(2204, [struct.pack("!i", 551)]),
            '{"+(integer,integer)"}',
            None,
        ),
        (
            "regclass array",
            "SELECT $1::regclass[]",
            2210,
            binary_array(2205, [struct.pack("!i", regclass_oid)]),
            "{wire_binary_regclass}",
            None,
        ),
        (
            "regnamespace array",
            "SELECT $1::regnamespace[]",
            4090,
            binary_array(4089, [struct.pack("!i", namespace_oid)]),
            "{public}",
            None,
        ),
        (
            "regrole array",
            "SELECT $1::regrole[]",
            4097,
            binary_array(4096, [struct.pack("!i", role_oid)]),
            "{" + role_name + "}",
            None,
        ),
        ("invalid regtype", "SELECT $1::regtype", 2206, b"\x00", None, "22P03"),
        ("json", "SELECT $1::json", 114, b'{"b": 1, "a": 2}', '{"b": 1, "a": 2}', None),
        ("jsonb", "SELECT $1::jsonb", 3802, b'\x01{"b": 1, "a": 2}', '{"a": 2, "b": 1}', None),
        ("enum", "SELECT $1::wire_binary_state", enum_oid, b"ready", "ready", None),
        ("domain", "SELECT $1::wire_binary_positive", domain_oid, struct.pack("!i", 7), "7", None),
        ("routine enum", "SELECT wire_binary_state_echo($1)", enum_oid, b"ready", "ready", None),
        ("routine domain", "SELECT wire_binary_positive_echo($1)", domain_oid, struct.pack("!i", 7), "7", None),
        ("routine composite", "SELECT wire_binary_coordinate_echo($1)", coordinate_oid, coordinate, "(4,8)", None),
        (
            "routine enum array",
            "SELECT wire_binary_state_array_echo($1)",
            enum_array_oid,
            binary_array(enum_oid, [b"ready", b"blocked"]),
            "{ready,blocked}",
            None,
        ),
        (
            "routine domain array",
            "SELECT wire_binary_positive_array_echo($1)",
            domain_array_oid,
            binary_array(domain_oid, [struct.pack("!i", 3), struct.pack("!i", 5)]),
            "{3,5}",
            None,
        ),
        (
            "routine composite array",
            "SELECT wire_binary_coordinate_array_echo($1)",
            coordinate_array_oid,
            binary_array(coordinate_oid, [coordinate]),
            "{\"(4,8)\"}",
            None,
        ),
        (
            "composite domain",
            "SELECT $1::wire_binary_coordinate_domain",
            coordinate_domain_oid,
            coordinate,
            "(4,8)",
            None,
        ),
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
            "shaped domain array",
            "SELECT $1::wire_binary_positive[]",
            domain_array_oid,
            binary_array_shaped(
                domain_oid, [2], [-2], [struct.pack("!i", 3), struct.pack("!i", 5)]
            ),
            "[-2:-1]={3,5}",
            None,
        ),
        (
            "shaped integer array",
            "SELECT $1::integer[]",
            1007,
            binary_array_shaped(
                23,
                [2, 2],
                [2, 4],
                [struct.pack("!i", value) for value in [1, 2, 3, 4]],
            ),
            "[2:3][4:5]={{1,2},{3,4}}",
            None,
        ),
        (
            "shaped enum array",
            "SELECT $1::wire_binary_state[]",
            enum_array_oid,
            binary_array_shaped(enum_oid, [2, 2], [0, 5], [b"ready", b"blocked", b"blocked", b"ready"]),
            "[0:1][5:6]={{ready,blocked},{blocked,ready}}",
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
        (
            "named composite array",
            "SELECT $1::wire_binary_coordinate[]",
            coordinate_array_oid,
            binary_array(coordinate_oid, [coordinate]),
            '{"(4,8)"}',
            None,
        ),
        (
            "composite domain array",
            "SELECT $1::wire_binary_coordinate_domain[]",
            coordinate_domain_array_oid,
            binary_array(coordinate_domain_oid, [coordinate]),
            '{"(4,8)"}',
            None,
        ),
        (
            "bit array",
            "SELECT $1::bit(5)[]",
            1561,
            binary_array(1560, [binary_bit("10110"), None, binary_bit("00111")]),
            "{10110,NULL,00111}",
            None,
        ),
        (
            "varbit array",
            "SELECT $1::varbit[]",
            1563,
            binary_array(1562, [binary_bit("1"), None, binary_bit("00111")]),
            "{1,NULL,00111}",
            None,
        ),
        (
            "unsigned oid array",
            "SELECT $1::oid[]",
            1028,
            binary_array(26, [struct.pack("!I", 1), None, struct.pack("!I", 4294967295)]),
            "{1,NULL,4294967295}",
            None,
        ),
        (
            "range array",
            "SELECT $1::int4range[]",
            3905,
            binary_array(3904, [binary_int4_range(1, 3), None, binary_int4_range(5, 7)]),
            '{"[1,3)",NULL,"[5,7)"}',
            None,
        ),
        (
            "multirange array",
            "SELECT $1::int4multirange[]",
            6150,
            binary_array(4451, [binary_multirange([binary_int4_range(1, 3)])]),
            '{"{[1,3)}"}',
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
        (
            "null required domain-array element",
            "SELECT $1::wire_binary_required[]",
            required_domain_array_oid,
            binary_array(required_domain_oid, [None]),
            None,
            "23502",
        ),
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
    null_domain_record = binary_record([(required_domain_oid, None)])
    messages = extended_binary_parameter(s, "SELECT $1::record", 2249, null_domain_record)
    check(
        "binary Bind record enforces nested domain not-null",
        has_sqlstate(messages, "23502"),
        messages,
    )
    # A domain over a composite remains a domain at the array wire boundary:
    # its array header names the domain OID while each element is a binary
    # record, never the stored composite text.
    query = "SELECT ARRAY[ROW(4,8)::wire_binary_coordinate]::wire_binary_coordinate_domain[]"
    parse = frontend_message(b"P", b"\x00" + query.encode() + b"\x00\x00\x00")
    bind = frontend_message(b"B", b"\x00\x00" + struct.pack("!hhh", 0, 0, 1) + struct.pack("!h", 1))
    describe = frontend_message(b"D", b"P\x00")
    execute = frontend_message(b"E", b"\x00\x00\x00\x00\x00")
    s.sendall(parse + bind + describe + execute + frontend_message(b"S"))
    messages = []
    while True:
        item = read_message(s)
        messages.append(item)
        if item[0] == b"Z":
            break
    description = next((payload for kind, payload in messages if kind == b"T"), None)
    row = next((payload for kind, payload in messages if kind == b"D"), None)
    expected_array = binary_array(coordinate_domain_oid, [coordinate])
    check(
        "binary Result preserves composite-domain array identity and records",
        description is not None
        and row_description_type_oids(description) == [coordinate_domain_array_oid]
        and row_description_formats(description) == [1]
        and row == b"\x00\x01" + struct.pack("!i", len(expected_array)) + expected_array,
        messages,
    )
    shaped_results = [
        (
            "enum array shape",
            "SELECT '[0:1][5:6]={{ready,blocked},{blocked,ready}}'::wire_binary_state[]",
            enum_array_oid,
            binary_array_shaped(
                enum_oid, [2, 2], [0, 5], [b"ready", b"blocked", b"blocked", b"ready"]
            ),
        ),
        (
            "domain array shape",
            "SELECT '[-2:-1]={3,5}'::wire_binary_positive[]",
            domain_array_oid,
            binary_array_shaped(domain_oid, [2], [-2], [struct.pack("!i", 3), struct.pack("!i", 5)]),
        ),
    ]
    for name, query, oid, expected in shaped_results:
        messages = extended_binary_result(s, query)
        description = next((payload for kind, payload in messages if kind == b"T"), None)
        row = next((payload for kind, payload in messages if kind == b"D"), None)
        check(
            f"binary Result preserves {name}",
            description is not None
            and row_description_type_oids(description) == [oid]
            and row_description_formats(description) == [1]
            and row == b"\x00\x01" + struct.pack("!i", len(expected)) + expected,
            messages,
        )
    routine_results = [
        ("enum", "SELECT wire_binary_state_echo('ready'::wire_binary_state)", enum_oid, b"ready"),
        ("domain base", "SELECT wire_binary_positive_echo(7::wire_binary_positive)", 23, struct.pack("!i", 7)),
        ("composite", "SELECT wire_binary_coordinate_echo(ROW(4,8)::wire_binary_coordinate)", coordinate_oid, coordinate),
        (
            "enum array",
            "SELECT wire_binary_state_array_echo(ARRAY['ready'::wire_binary_state])",
            enum_array_oid,
            binary_array(enum_oid, [b"ready"]),
        ),
        (
            "domain array",
            "SELECT wire_binary_positive_array_echo(ARRAY[7::wire_binary_positive])",
            domain_array_oid,
            binary_array(domain_oid, [struct.pack("!i", 7)]),
        ),
        (
            "composite array",
            "SELECT wire_binary_coordinate_array_echo(ARRAY[ROW(4,8)::wire_binary_coordinate])",
            coordinate_array_oid,
            binary_array(coordinate_oid, [coordinate]),
        ),
    ]
    for name, query, oid, expected in routine_results:
        messages = extended_binary_result(s, query)
        description = next((payload for kind, payload in messages if kind == b"T"), None)
        row = next((payload for kind, payload in messages if kind == b"D"), None)
        check(
            f"binary Result describes routine {name}",
            description is not None
            and row_description_type_oids(description) == [oid]
            and row_description_formats(description) == [1]
            and row == b"\x00\x01" + struct.pack("!i", len(expected)) + expected,
            messages,
        )
    catalog_reference_results = [
        ("regtype", "SELECT ARRAY['integer'::regtype]", 2211, 2206, 23),
        ("regproc", "SELECT ARRAY['wire_binary_routine'::regproc]", 1008, 24, routine_oid),
        (
            "regprocedure",
            "SELECT ARRAY['wire_binary_routine(integer)'::regprocedure]",
            2207,
            2202,
            routine_oid,
        ),
        ("regoper", "SELECT ARRAY[551::regoper]", 2208, 2203, 551),
        (
            "regoperator",
            "SELECT ARRAY['+(integer,integer)'::regoperator]",
            2209,
            2204,
            551,
        ),
        (
            "regclass",
            "SELECT ARRAY['wire_binary_regclass'::regclass]",
            2210,
            2205,
            regclass_oid,
        ),
        ("regnamespace", "SELECT ARRAY['public'::regnamespace]", 4090, 4089, namespace_oid),
        ("regrole", "SELECT ARRAY[current_user::regrole]", 4097, 4096, role_oid),
    ]
    for name, query, array_oid, element_oid, value_oid in catalog_reference_results:
        messages = extended_binary_result(s, query)
        description = next((payload for kind, payload in messages if kind == b"T"), None)
        row = next((payload for kind, payload in messages if kind == b"D"), None)
        expected = binary_array(element_oid, [struct.pack("!i", value_oid)])
        check(
            f"binary Result preserves {name} array identity and element OID",
            description is not None
            and row_description_type_oids(description) == [array_oid]
            and row_description_formats(description) == [1]
            and row == b"\x00\x01" + struct.pack("!i", len(expected)) + expected,
            messages,
        )
    messages = extended_binary_result(s, "SELECT ARRAY[1::oid, 4294967295::oid]")
    description = next((payload for kind, payload in messages if kind == b"T"), None)
    row = next((payload for kind, payload in messages if kind == b"D"), None)
    expected = binary_array(26, [struct.pack("!I", 1), struct.pack("!I", 4294967295)])
    check(
        "binary Result preserves unsigned oid array identity and values",
        description is not None
        and row_description_type_oids(description) == [1028]
        and row_description_formats(description) == [1]
        and row == b"\x00\x01" + struct.pack("!i", len(expected)) + expected,
        messages,
    )
    range_array_results = [
        ("range", "SELECT ARRAY['[1,3)'::int4range]", 3905, 3904, binary_int4_range(1, 3)),
        (
            "multirange",
            "SELECT ARRAY['{[1,3)}'::int4multirange]",
            6150,
            4451,
            binary_multirange([binary_int4_range(1, 3)]),
        ),
    ]
    for name, query, array_oid, element_oid, value in range_array_results:
        messages = extended_binary_result(s, query)
        description = next((payload for kind, payload in messages if kind == b"T"), None)
        row = next((payload for kind, payload in messages if kind == b"D"), None)
        expected = binary_array(element_oid, [value])
        check(
            f"binary Result preserves {name} array identity and nested send form",
            description is not None
            and row_description_type_oids(description) == [array_oid]
            and row_description_formats(description) == [1]
            and row == b"\x00\x01" + struct.pack("!i", len(expected)) + expected,
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
    messages = extended_binary_result(
        s,
        "SELECT wire_values_from(1), generate_series(10,12)",
    )
    description = next((payload for kind, payload in messages if kind == b"T"), None)
    rows = [payload for kind, payload in messages if kind == b"D"]
    expected_rows = [
        b"\x00\x02\x00\x00\x00\x04" + struct.pack("!i", value)
        + b"\x00\x00\x00\x04" + struct.pack("!i", generated)
        for value, generated in [(40, 10), (41, 11)]
    ] + [
        b"\x00\x02\xff\xff\xff\xff\x00\x00\x00\x04" + struct.pack("!i", 12)
    ]
    check(
        "binary Result locksteps a catalog SRF without losing its integer type",
        description is not None
        and row_description_type_oids(description) == [23, 23]
        and row_description_formats(description) == [1, 1]
        and rows == expected_rows,
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
    loop_setup = simple_query(
        s,
        "CREATE TABLE wire_loop_target (id integer PRIMARY KEY, value integer); "
        "CREATE FUNCTION wire_loop_program() RETURNS trigger LANGUAGE plpgsql AS "
        "'DECLARE item integer; total integer := 0; BEGIN "
        "FOR item IN REVERSE 5..1 BY 2 LOOP "
        "CONTINUE WHEN item = 3; total := total + item; EXIT WHEN item = 1; END LOOP; "
        "NEW.value := NEW.value + total; RETURN NEW; END'; "
        "CREATE TRIGGER wire_loop_before BEFORE INSERT ON wire_loop_target "
        "FOR EACH ROW EXECUTE FUNCTION wire_loop_program(); "
        "INSERT INTO wire_loop_target VALUES (1, 10)",
    )
    check("raw wire: numeric trigger loop setup succeeds", not any(kind == b"E" for kind, _ in loop_setup), loop_setup)
    check(
        "raw wire: numeric trigger loop control flow is visible",
        first_text_row(simple_query(s, "SELECT value FROM wire_loop_target")) == "16",
    )
    structured = simple_query(
        s,
        "CREATE TABLE wire_structured_loop_target (id integer PRIMARY KEY, value integer); "
        "CREATE FUNCTION wire_structured_loop_program() RETURNS trigger LANGUAGE plpgsql AS "
        "'DECLARE item integer := 0; total integer := 0; stop integer := 0; BEGIN "
        "WHILE item < 5 LOOP item := item + 1; CONTINUE WHEN item = 2; "
        "total := total + item; END LOOP; "
        "LOOP stop := stop + 1; CONTINUE WHEN stop = 1; total := total + stop; "
        "EXIT WHEN stop = 3; END LOOP; NEW.value := total; RETURN NEW; END'; "
        "CREATE TRIGGER wire_structured_loop_before BEFORE INSERT ON wire_structured_loop_target "
        "FOR EACH ROW EXECUTE FUNCTION wire_structured_loop_program(); "
        "INSERT INTO wire_structured_loop_target VALUES (1, 0)",
    )
    check("raw wire: structured trigger loop setup succeeds", not any(kind == b"E" for kind, _ in structured), structured)
    check(
        "raw wire: while and unconditional trigger loops are visible",
        first_text_row(simple_query(s, "SELECT value FROM wire_structured_loop_target")) == "18",
    )
    labelled = simple_query(
        s,
        "CREATE TABLE wire_labelled_loop_target (id integer PRIMARY KEY, value integer); "
        "CREATE FUNCTION wire_labelled_loop_program() RETURNS trigger LANGUAGE plpgsql AS "
        "'DECLARE round integer := 0; total integer := 0; BEGIN "
        "<<outer>> LOOP round := round + 1; EXIT outer WHEN round = 3; "
        "<<inner>> LOOP total := total + 1; CONTINUE outer; END LOOP; END LOOP; "
        "NEW.value := total; RETURN NEW; END'; "
        "CREATE TRIGGER wire_labelled_loop_before BEFORE INSERT ON wire_labelled_loop_target "
        "FOR EACH ROW EXECUTE FUNCTION wire_labelled_loop_program(); "
        "INSERT INTO wire_labelled_loop_target VALUES (1, 0)",
    )
    check("raw wire: labelled trigger loop setup succeeds", not any(kind == b"E" for kind, _ in labelled), labelled)
    check(
        "raw wire: parser-resolved labelled loop control is visible",
        first_text_row(simple_query(s, "SELECT value FROM wire_labelled_loop_target")) == "2",
    )
    records = simple_query(
        s,
        "CREATE TABLE wire_record_loop_target (id integer PRIMARY KEY, value integer); "
        "CREATE TABLE wire_record_loop_source (id integer PRIMARY KEY, delta integer); "
        "INSERT INTO wire_record_loop_source VALUES (1, 4), (2, 7); "
        "CREATE FUNCTION wire_record_loop_program() RETURNS trigger LANGUAGE plpgsql AS "
        "'DECLARE entry record; total integer := 0; BEGIN "
        "FOR entry IN SELECT id, delta FROM wire_record_loop_source ORDER BY id LOOP "
        "total := total + entry.id + entry.delta; END LOOP; NEW.value := total; RETURN NEW; END'; "
        "CREATE TRIGGER wire_record_loop_before BEFORE INSERT ON wire_record_loop_target "
        "FOR EACH ROW EXECUTE FUNCTION wire_record_loop_program(); "
        "INSERT INTO wire_record_loop_target VALUES (1, 0)",
    )
    check("raw wire: record trigger loop setup succeeds", not any(kind == b"E" for kind, _ in records), records)
    check(
        "raw wire: record trigger loop fields are visible",
        first_text_row(simple_query(s, "SELECT value FROM wire_record_loop_target")) == "14",
    )
    s.close()


def test_trigger_enablement_modes_over_raw_wire():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(
        s,
        "CREATE TABLE wire_trigger_modes (id integer PRIMARY KEY); "
        "CREATE FUNCTION wire_trigger_mode_fn() RETURNS trigger LANGUAGE plpgsql AS "
        "'BEGIN RETURN NEW; END'; "
        "CREATE TRIGGER wire_trigger_mode BEFORE INSERT ON wire_trigger_modes "
        "FOR EACH ROW EXECUTE FUNCTION wire_trigger_mode_fn(); "
        "ALTER TABLE wire_trigger_modes ENABLE REPLICA TRIGGER wire_trigger_mode; "
        "ALTER TABLE wire_trigger_modes ENABLE ALWAYS TRIGGER wire_trigger_mode",
    )
    check("raw wire: trigger mode DDL completes", not any(kind == b"E" for kind, _ in setup), setup)
    check(
        "raw wire: trigger mode reaches pg_trigger",
        first_text_row(
            simple_query(
                s,
                "SELECT tgenabled FROM pg_trigger WHERE tgname = 'wire_trigger_mode'",
            )
        )
        == "A",
    )
    disabled = simple_query(
        s,
        "ALTER TABLE wire_trigger_modes DISABLE TRIGGER wire_trigger_mode; "
        "SELECT tgenabled FROM pg_trigger WHERE tgname = 'wire_trigger_mode'",
    )
    check(
        "raw wire: disabled trigger mode reaches pg_trigger",
        first_text_row(disabled) == "D",
        disabled,
    )
    selectors = simple_query(
        s,
        "CREATE TRIGGER wire_trigger_mode_second BEFORE INSERT ON wire_trigger_modes "
        "FOR EACH ROW EXECUTE FUNCTION wire_trigger_mode_fn(); "
        "ALTER TABLE wire_trigger_modes DISABLE TRIGGER ALL; "
        "SELECT count(*) FROM pg_trigger WHERE tgrelid = 'wire_trigger_modes'::regclass "
        "AND tgenabled = 'D'; "
        "ALTER TABLE wire_trigger_modes ENABLE TRIGGER USER; "
        "SELECT count(*) FROM pg_trigger WHERE tgrelid = 'wire_trigger_modes'::regclass "
        "AND tgenabled = 'O'",
    )
    check(
        "raw wire: ALL and USER trigger selectors reach pg_trigger",
        [first_text_row([message]) for message in selectors if message[0] == b"D"] == ["2", "2"],
        selectors,
    )
    s.close()


def test_type_schema_moves_over_raw_wire():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(
        s,
        "CREATE SCHEMA wire_moved_types; "
        "CREATE TYPE wire_moved_state AS ENUM ('ready', 'blocked'); "
        "CREATE TYPE wire_moved_point AS (x integer, y integer); "
        "CREATE TABLE wire_moved_values (state wire_moved_state, point wire_moved_point); "
        "INSERT INTO wire_moved_values VALUES ('ready', ROW(3,4)::wire_moved_point); "
        "ALTER TYPE wire_moved_state SET SCHEMA wire_moved_types; "
        "ALTER TYPE wire_moved_point SET SCHEMA wire_moved_types",
    )
    check("raw wire: type schema moves succeed", not any(kind == b"E" for kind, _ in setup), setup)
    check(
        "raw wire: moved types retain existing values",
        first_text_row(simple_query(s, "SELECT state::text || ':' || (point).x || ':' || (point).y FROM wire_moved_values"))
        == "ready:3:4",
    )
    s.close()


def test_user_defined_aggregate_over_named_binary_portal():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(
        s,
        "CREATE FUNCTION wire_aggregate_state(state bigint, value integer) "
        "RETURNS bigint LANGUAGE SQL AS 'SELECT coalesce(state, 0) + value'; "
        "CREATE FUNCTION wire_aggregate_final(state bigint) "
        "RETURNS bigint LANGUAGE SQL AS 'SELECT state * 2'; "
        "CREATE FUNCTION wire_aggregate_first_state(state anyelement, value anyelement) "
        "RETURNS anyelement LANGUAGE SQL AS 'SELECT coalesce(state, value)'; "
        "CREATE AGGREGATE wire_total(integer) "
        "(SFUNC = wire_aggregate_state, STYPE = bigint, FINALFUNC = wire_aggregate_final); "
        "CREATE AGGREGATE wire_first(anyelement) "
        "(SFUNC = wire_aggregate_first_state, STYPE = anyelement)",
    )
    check(
        "raw wire: user-defined aggregate setup succeeds",
        not any(kind == b"E" for kind, _ in setup),
        setup,
    )
    parse = frontend_message(
        b"P",
        b"wire_aggregate_statement\x00SELECT wire_total($1)\x00"
        + struct.pack("!hi", 1, 23),
    )
    bind = frontend_message(
        b"B",
        b"wire_aggregate_portal\x00wire_aggregate_statement\x00"
        + struct.pack("!hhh", 1, 1, 1)
        + struct.pack("!ii", 4, 7)
        + struct.pack("!hh", 1, 1),
    )
    describe = frontend_message(b"D", b"Pwire_aggregate_portal\x00")
    execute = frontend_message(b"E", b"wire_aggregate_portal\x00\x00\x00\x00\x00")
    s.sendall(parse + bind + describe + execute + frontend_message(b"S"))
    messages = []
    while True:
        item = read_message(s)
        messages.append(item)
        if item[0] == b"Z":
            break
    description = next((payload for kind, payload in messages if kind == b"T"), None)
    data = next((payload for kind, payload in messages if kind == b"D"), None)
    check(
        "raw wire: aggregate portal preserves bigint binary result metadata",
        description is not None
        and row_description_type_oids(description) == [20]
        and row_description_formats(description) == [1],
        messages,
    )
    check(
        "raw wire: aggregate portal executes a binary int4 Bind",
        data == b"\x00\x01\x00\x00\x00\x08" + struct.pack("!q", 14),
        messages,
    )
    parse = frontend_message(
        b"P",
        b"wire_polymorphic_aggregate_statement\x00SELECT wire_first($1)\x00"
        + struct.pack("!hi", 1, 23),
    )
    bind = frontend_message(
        b"B",
        b"wire_polymorphic_aggregate_portal\x00wire_polymorphic_aggregate_statement\x00"
        + struct.pack("!hhh", 1, 1, 1)
        + struct.pack("!ii", 4, 9)
        + struct.pack("!hh", 1, 1),
    )
    describe = frontend_message(b"D", b"Pwire_polymorphic_aggregate_portal\x00")
    execute = frontend_message(b"E", b"wire_polymorphic_aggregate_portal\x00\x00\x00\x00\x00")
    s.sendall(parse + bind + describe + execute + frontend_message(b"S"))
    messages = []
    while True:
        item = read_message(s)
        messages.append(item)
        if item[0] == b"Z":
            break
    description = next((payload for kind, payload in messages if kind == b"T"), None)
    data = next((payload for kind, payload in messages if kind == b"D"), None)
    check(
        "raw wire: polymorphic aggregate resolves Bind and Result as int4",
        description is not None
        and row_description_type_oids(description) == [23]
        and row_description_formats(description) == [1]
        and data == b"\x00\x01\x00\x00\x00\x04" + struct.pack("!i", 9),
        messages,
    )
    s.close()


def test_partitioned_tables_over_raw_wire():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(
        s,
        "CREATE TABLE wire_partition_parent (id integer PRIMARY KEY, note text) PARTITION BY RANGE (id); "
        "CREATE TABLE wire_partition_low PARTITION OF wire_partition_parent FOR VALUES FROM (0) TO (10); "
        "CREATE TABLE wire_partition_high PARTITION OF wire_partition_parent FOR VALUES FROM (10) TO (20); "
        "INSERT INTO wire_partition_parent VALUES (1, 'low')",
    )
    check("raw wire: partition DDL and routed insert complete", not any(kind == b"E" for kind, _ in setup), setup)
    moved = simple_query(
        s,
        "UPDATE wire_partition_parent SET id = 11, note = 'high' WHERE id = 1; "
        "SELECT id || ':' || note FROM wire_partition_parent",
    )
    check(
        "raw wire: partition-key update moves the physical row",
        first_text_row(moved) == "11:high",
        moved,
    )
    tree = simple_query(
        s,
        "CREATE TABLE wire_partition_tree (id integer, region integer) PARTITION BY RANGE (id); "
        "CREATE TABLE wire_partition_mid PARTITION OF wire_partition_tree FOR VALUES FROM (0) TO (100) PARTITION BY LIST (region); "
        "CREATE TABLE wire_partition_east PARTITION OF wire_partition_mid FOR VALUES IN (1); "
        "CREATE TABLE wire_partition_other (id integer, region integer); "
        "ALTER TABLE wire_partition_mid ATTACH PARTITION wire_partition_other DEFAULT; "
        "INSERT INTO wire_partition_tree VALUES (10, 1), (20, 2); "
        "ALTER TABLE wire_partition_mid DETACH PARTITION wire_partition_other; "
        "SELECT id || ':' || region FROM wire_partition_other",
    )
    check(
        "raw wire: subpartition attach and detach retain the physical row",
        first_text_row(tree) == "20:2",
        tree,
    )
    bound = simple_query(
        s,
        "SELECT relpartbound FROM pg_class WHERE relname = 'wire_partition_east'",
    )
    description = next(payload for kind, payload in bound if kind == b"T")
    check(
        "raw wire: partition bounds retain pg_node_tree metadata",
        row_description_type_oids(description) == [194]
        and first_text_row(bound) == "FOR VALUES IN (1)",
        bound,
    )
    cleanup = simple_query(
        s,
        "DROP TABLE wire_partition_east, wire_partition_other, wire_partition_mid, wire_partition_tree",
    )
    check(
        "raw wire: partition hierarchy cleanup succeeds",
        not any(kind == b"E" for kind, _ in cleanup),
        cleanup,
    )
    s.close()


def test_deferred_constraint_commit_over_raw_wire():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(
        s,
        "CREATE TABLE wire_deferred_constraint (value integer, "
        "CONSTRAINT wire_deferred_key UNIQUE (value) DEFERRABLE INITIALLY DEFERRED); "
        "INSERT INTO wire_deferred_constraint VALUES (1); BEGIN",
    )
    check(
        "raw wire: deferred constraint setup succeeds",
        not any(kind == b"E" for kind, _ in setup),
        setup,
    )
    inserted = extended_binary_parameter(
        s,
        "INSERT INTO wire_deferred_constraint VALUES ($1)",
        23,
        struct.pack("!i", 1),
    )
    check(
        "raw wire: binary Bind can raise a deferred obligation",
        not any(kind == b"E" for kind, _ in inserted)
        and inserted[-1] == (b"Z", b"T"),
        inserted,
    )
    committed = simple_query(s, "COMMIT")
    check(
        "raw wire: commit reports the deferred SQLSTATE and leaves idle state",
        has_sqlstate(committed, "23505") and committed[-1] == (b"Z", b"I"),
        committed,
    )
    s.close()


def test_constraint_trigger_savepoint_and_partition_clone_over_raw_wire():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(
        s,
        "CREATE TABLE wire_constraint_trigger_target (id integer PRIMARY KEY); "
        "CREATE TABLE wire_constraint_trigger_audit (id integer); "
        "CREATE FUNCTION wire_constraint_trigger_fn() RETURNS trigger LANGUAGE plpgsql AS "
        "'BEGIN INSERT INTO wire_constraint_trigger_audit VALUES (NEW.id); RETURN NEW; END'; "
        "CREATE CONSTRAINT TRIGGER wire_constraint_trigger AFTER INSERT "
        "ON wire_constraint_trigger_target DEFERRABLE INITIALLY DEFERRED "
        "FOR EACH ROW EXECUTE FUNCTION wire_constraint_trigger_fn(); "
        "BEGIN; INSERT INTO wire_constraint_trigger_target VALUES (1); "
        "SAVEPOINT queued; SET CONSTRAINTS wire_constraint_trigger IMMEDIATE; "
        "ROLLBACK TO SAVEPOINT queued; COMMIT",
    )
    check(
        "raw wire: savepoint restores a completed constraint-trigger event",
        not any(kind == b"E" for kind, _ in setup)
        and first_text_row(simple_query(s, "SELECT id FROM wire_constraint_trigger_audit")) == "1",
        setup,
    )

    partition = simple_query(
        s,
        "CREATE TABLE wire_clone_root (id integer) PARTITION BY RANGE (id); "
        "CREATE TABLE wire_clone_low PARTITION OF wire_clone_root FOR VALUES FROM (0) TO (100); "
        "CREATE FUNCTION wire_clone_fn() RETURNS trigger LANGUAGE plpgsql AS "
        "'BEGIN INSERT INTO wire_constraint_trigger_audit VALUES (NEW.id); RETURN NEW; END'; "
        "CREATE TRIGGER wire_clone_after AFTER INSERT ON wire_clone_root "
        "FOR EACH ROW EXECUTE FUNCTION wire_clone_fn(); "
        "ALTER TABLE ONLY wire_clone_root DISABLE TRIGGER wire_clone_after; "
        "INSERT INTO wire_clone_root VALUES (2); "
        "ALTER TABLE wire_clone_low DISABLE TRIGGER wire_clone_after; "
        "INSERT INTO wire_clone_root VALUES (3)",
    )
    clone_rows = simple_query(
        s,
        "SELECT c.relname || ':' || t.tgenabled FROM pg_trigger t "
        "JOIN pg_class c ON c.oid = t.tgrelid WHERE t.tgname = 'wire_clone_after' "
        "ORDER BY c.relname",
    )
    check(
        "raw wire: ONLY and leaf clone firing modes are independent",
        not any(kind == b"E" for kind, _ in partition)
        and [first_text_row([message]) for message in clone_rows if message[0] == b"D"]
        == ["wire_clone_low:D", "wire_clone_root:D"]
        and first_text_row(
            simple_query(s, "SELECT string_agg(id::text, ',' ORDER BY id) FROM wire_constraint_trigger_audit")
        )
        == "1,2",
        partition + clone_rows,
    )
    s.close()


def test_row_security_over_named_statement_and_portal():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(
        s,
        "CREATE ROLE wire_rls_client; "
        "CREATE TABLE wire_rls_target (id integer PRIMARY KEY, tenant text); "
        "INSERT INTO wire_rls_target VALUES (1, 'wire_rls_client'), (2, 'other'); "
        "ALTER TABLE wire_rls_target ENABLE ROW LEVEL SECURITY; "
        "CREATE POLICY wire_rls_rows ON wire_rls_target TO wire_rls_client "
        "USING (tenant = 'wire_rls_client'); "
        "GRANT SELECT ON wire_rls_target TO wire_rls_client; "
        "SET ROLE wire_rls_client",
    )
    check("raw wire: row-security setup succeeds", not any(kind == b"E" for kind, _ in setup), setup)
    parse = frontend_message(
        b"P",
        b"wire_rls_statement\x00SELECT id FROM wire_rls_target ORDER BY id\x00\x00\x00",
    )
    bind = frontend_message(
        b"B",
        b"wire_rls_portal\x00wire_rls_statement\x00" + struct.pack("!hhh", 0, 0, 0),
    )
    describe = frontend_message(b"D", b"Pwire_rls_portal\x00")
    execute = frontend_message(b"E", b"wire_rls_portal\x00\x00\x00\x00\x00")
    s.sendall(parse + bind + describe + execute + frontend_message(b"S"))
    out = []
    while True:
        item = read_message(s)
        out.append(item)
        if item[0] == b"Z":
            break
    description = next((payload for kind, payload in out if kind == b"T"), None)
    check(
        "raw wire: named portal applies row security before returning rows",
        description is not None
        and row_description_type_oids(description) == [23]
        and [first_text_row([message]) for message in out if message[0] == b"D"] == ["1"],
        out,
    )
    s.close()


def test_nested_materialized_cte_named_portal():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(s, "CREATE SEQUENCE wire_cte_sequence")
    check("raw wire: CTE sequence setup succeeds", not any(kind == b"E" for kind, _ in setup), setup)
    query = (
        "WITH outer_value AS MATERIALIZED ("
        "SELECT nextval('wire_cte_sequence') AS marker, $1::varchar(5) AS label) "
        "SELECT nested.left_marker, nested.right_marker, nested.label FROM ("
        "WITH inner_value AS MATERIALIZED (SELECT marker, label FROM outer_value) "
        "SELECT l.marker AS left_marker, r.marker AS right_marker, l.label "
        "FROM inner_value AS l CROSS JOIN inner_value AS r) AS nested"
    )
    parse = frontend_message(
        b"P",
        b"wire_cte_statement\x00" + query.encode() + b"\x00" + struct.pack("!hI", 1, 1043),
    )
    bind = frontend_message(
        b"B",
        b"wire_cte_portal\x00wire_cte_statement\x00"
        + struct.pack("!hh", 0, 1)
        + struct.pack("!i", 3)
        + b"abc"
        + struct.pack("!h", 0),
    )
    describe = frontend_message(b"D", b"Pwire_cte_portal\x00")
    execute = frontend_message(b"E", b"wire_cte_portal\x00\x00\x00\x00\x00")
    s.sendall(parse + bind + describe + execute + frontend_message(b"S"))
    out = []
    while True:
        item = read_message(s)
        out.append(item)
        if item[0] == b"Z":
            break
    description = next((payload for kind, payload in out if kind == b"T"), None)
    check(
        "raw wire: nested materialized CTE retains typed portal metadata and evaluates once",
        description is not None
        and row_description_type_oids(description) == [20, 20, 1043]
        and row_description_type_modifiers(description) == [-1, -1, 9]
        and [text_row_fields(payload) for kind, payload in out if kind == b"D"]
        == [["1", "1", "abc"]],
        out,
    )
    s.close()


def test_format_models_over_named_portal():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(s, "SET TimeZone='-05:00'")
    check("raw wire: formatting session setup succeeds", not any(kind == b"E" for kind, _ in setup), setup)
    query = (
        "SELECT to_number($1,$2)::text, to_char($3::timestamptz,$4), "
        "to_char(interval '2 years 3 mons 15 days 36:07:05.123456',$5)"
    )
    parse = frontend_message(
        b"P",
        b"wire_format_statement\x00"
        + query.encode()
        + b"\x00"
        + struct.pack("!hIIIII", 5, 25, 25, 1184, 25, 25),
    )
    values = [
        b"XIV",
        b"RN",
        b"2024-02-29 23:07:05.123456-05",
        b"YYYY-MM-DD HH24:MI:SS.US OF",
        b"YYYY|MM|DDD|DD|HH24|MI|SS.MS",
    ]
    body = b"wire_format_portal\x00wire_format_statement\x00" + struct.pack("!hh", 0, len(values))
    for value in values:
        body += struct.pack("!i", len(value)) + value
    body += struct.pack("!hhhh", 3, 1, 0, 1)
    bind = frontend_message(b"B", body)
    describe = frontend_message(b"D", b"Pwire_format_portal\x00")
    execute = frontend_message(b"E", b"wire_format_portal\x00\x00\x00\x00\x00")
    s.sendall(parse + bind + describe + execute + frontend_message(b"S"))
    output = []
    while True:
        item = read_message(s)
        output.append(item)
        if item[0] == b"Z":
            break
    description = next((payload for kind, payload in output if kind == b"T"), None)
    row = next((payload for kind, payload in output if kind == b"D"), None)
    check(
        "raw wire: parsed format models retain text OIDs and requested formats",
        description is not None
        and row_description_type_oids(description) == [25, 25, 25]
        and row_description_formats(description) == [1, 0, 1],
        output,
    )
    check(
        "raw wire: named portal applies PostgreSQL numeric, zone, and interval models",
        row is not None
        and text_row_fields(row)
        == [
            "14",
            "2024-03-01 09:07:05.123456 +05",
            "0002|03|825|15|36|07|05.123",
        ],
        output,
    )
    s.close()


def test_table_sample_binary_bind_and_named_portal():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(
        s,
        "CREATE TABLE wire_sample_source (id integer PRIMARY KEY); "
        "INSERT INTO wire_sample_source SELECT value FROM generate_series(1,20) value",
    )
    check(
        "raw wire: TABLESAMPLE source setup succeeds",
        not any(kind == b"E" for kind, _ in setup),
        setup,
    )
    query = (
        "SELECT id FROM wire_sample_source "
        "TABLESAMPLE BERNOULLI ($1) REPEATABLE ($2) ORDER BY id"
    )
    parse = frontend_message(
        b"P",
        b"wire_sample_statement\x00" + query.encode() + b"\x00" + struct.pack("!h", 0),
    )
    bind_body = (
        b"wire_sample_portal\x00wire_sample_statement\x00"
        + struct.pack("!hhh", 2, 1, 1)
        + struct.pack("!h", 2)
        + struct.pack("!if", 4, 100.0)
        + struct.pack("!id", 8, 42.0)
        + struct.pack("!hh", 1, 1)
    )
    bind = frontend_message(b"B", bind_body)
    describe = frontend_message(b"D", b"Pwire_sample_portal\x00")
    execute_first = frontend_message(b"E", b"wire_sample_portal\x00" + struct.pack("!i", 7))
    execute_rest = frontend_message(b"E", b"wire_sample_portal\x00" + struct.pack("!i", 0))
    s.sendall(parse + bind + describe + execute_first + execute_rest + frontend_message(b"S"))
    output = []
    while True:
        item = read_message(s)
        output.append(item)
        if item[0] == b"Z":
            break
    description = next((payload for kind, payload in output if kind == b"T"), None)
    rows = [payload for kind, payload in output if kind == b"D"]
    check(
        "raw wire: inferred TABLESAMPLE parameters accept binary float4/float8 Bind values",
        not any(kind == b"E" for kind, _ in output)
        and description is not None
        and row_description_type_oids(description) == [23]
        and row_description_formats(description) == [1],
        output,
    )
    check(
        "raw wire: sampled named portal suspends and resumes without resampling",
        len(rows) == 20
        and b"s" in [kind for kind, _ in output]
        and rows[0] == b"\x00\x01\x00\x00\x00\x04" + struct.pack("!i", 1)
        and rows[-1] == b"\x00\x01\x00\x00\x00\x04" + struct.pack("!i", 20),
        output,
    )
    s.close()


def test_join_using_alias_grouping_quantifier_binary_portal():
    s = connect()
    s.sendall(startup_payload(0))
    drain_startup(s)
    setup = simple_query(
        s,
        "CREATE TABLE wire_using_left (id integer, payload text); "
        "CREATE TABLE wire_using_right (id integer, payload text); "
        "INSERT INTO wire_using_left VALUES (1,'a'),(1,'b'),(2,'c'); "
        "INSERT INTO wire_using_right VALUES (1,'x'),(2,'y')",
    )
    check(
        "raw wire: JOIN USING alias source setup succeeds",
        not any(kind == b"E" for kind, _ in setup),
        setup,
    )
    query = (
        "SELECT merged.id, count(*) FROM wire_using_left "
        "JOIN wire_using_right USING (id) AS merged "
        "GROUP BY DISTINCT GROUPING SETS ((merged.id),(merged.id)) "
        "HAVING count(*) >= $1 ORDER BY merged.id USING >"
    )
    parse = frontend_message(
        b"P",
        b"wire_using_statement\x00" + query.encode() + b"\x00" + struct.pack("!h", 0),
    )
    bind_body = b"wire_using_portal\x00wire_using_statement\x00"
    bind_body += struct.pack("!hh", 1, 1)
    bind_body += struct.pack("!hiq", 1, 8, 1)
    bind_body += struct.pack("!hh", 1, 1)
    bind = frontend_message(b"B", bind_body)
    describe = frontend_message(b"D", b"Pwire_using_portal\x00")
    execute_first = frontend_message(b"E", b"wire_using_portal\x00" + struct.pack("!i", 1))
    execute_rest = frontend_message(b"E", b"wire_using_portal\x00" + struct.pack("!i", 0))
    s.sendall(parse + bind + describe + execute_first + execute_rest + frontend_message(b"S"))
    output = []
    while True:
        item = read_message(s)
        output.append(item)
        if item[0] == b"Z":
            break
    description = next((payload for kind, payload in output if kind == b"T"), None)
    rows = [payload for kind, payload in output if kind == b"D"]
    check(
        "raw wire: merged USING alias and grouping quantifier retain binary metadata",
        not any(kind == b"E" for kind, _ in output)
        and description is not None
        and row_description_type_oids(description) == [23, 20]
        and row_description_formats(description) == [1, 1],
        output,
    )
    check(
        "raw wire: merged grouping portal suspends and resumes in USING order",
        len(rows) == 2
        and b"s" in [kind for kind, _ in output]
        and rows[0]
        == b"\x00\x02\x00\x00\x00\x04"
        + struct.pack("!i", 2)
        + b"\x00\x00\x00\x08"
        + struct.pack("!q", 1)
        and rows[1]
        == b"\x00\x02\x00\x00\x00\x04"
        + struct.pack("!i", 1)
        + b"\x00\x00\x00\x08"
        + struct.pack("!q", 2),
        output,
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

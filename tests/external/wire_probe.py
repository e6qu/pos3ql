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


def standby_status(end_lsn):
    return frontend_message(b"d", b"r" + struct.pack("!QQQQB", end_lsn, end_lsn, end_lsn, 0, 0))


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
        "DROP PUBLICATION IF EXISTS wire_replication_pub; "
        "DROP TABLE IF EXISTS wire_replication; "
        "CREATE TABLE wire_replication (id integer); "
        "CREATE PUBLICATION wire_replication_pub FOR TABLE wire_replication",
    )

    stream = connect()
    stream.sendall(startup_payload(0, parameters=(("replication", "database"),)))
    drain_startup(stream)
    simple_query(stream, "CREATE_REPLICATION_SLOT wire_replication_slot LOGICAL pgoutput")
    stream.sendall(
        frontend_message(
            b"Q",
            b"START_REPLICATION SLOT wire_replication_slot LOGICAL 0/0 "
            b"(proto_version '1', publication_names 'wire_replication_pub')\x00",
        )
    )
    kind, payload = read_message(stream)
    check("pgoutput START_REPLICATION enters CopyBoth", kind == b"W", (kind, payload))

    simple_query(setup, "INSERT INTO wire_replication VALUES (42)")
    insert = None
    for _ in range(64):
        kind, payload = read_message(stream)
        if kind == b"d" and len(payload) > 25 and payload[:1] == b"w" and payload[25:26] == b"I":
            insert = payload
        if kind == b"d" and len(payload) > 25 and payload[:1] == b"w" and payload[25:26] == b"C":
            end_lsn = struct.unpack("!Q", payload[9:17])[0]
            stream.sendall(standby_status(end_lsn))
            if insert is not None:
                break
    check(
        "pgoutput defaults to text tuples unless binary is negotiated",
        insert is not None and insert[33:34] == b"t" and insert[34:38] == struct.pack("!i", 2) and insert[38:40] == b"42",
        insert,
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

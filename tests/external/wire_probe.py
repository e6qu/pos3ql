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


def startup_payload(minor, user=b"probe"):
    body = struct.pack("!i", (3 << 16) | minor)
    body += b"user\x00" + user + b"\x00\x00"
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
    body += b"user\x00probe\x00_pq_.made_up_option\x00yes\x00\x00"
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

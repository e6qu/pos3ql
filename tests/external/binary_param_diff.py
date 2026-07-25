#!/usr/bin/env python3
"""Extended-protocol binary parameter differential: real PostgreSQL vs pos3ql.

psycopg's `%b` placeholder forces a parameter onto the wire in binary format, so
these bind composite parameters (arrays, ranges, multiranges) in binary and
check the value the server decodes and echoes back matches PostgreSQL exactly.
Scalar binary params were already exercised elsewhere; the point here is the
composite receive codec reached through the parameter path.

  binary_param_diff.py --pg PORT --p3 PORT [--host HOST]
"""
import argparse
import sys

try:
    import psycopg
    from psycopg.types.range import Range
    from psycopg.types.multirange import Multirange
except ImportError as e:
    print("psycopg not installed:", e, file=sys.stderr)
    sys.exit(2)


def connect(host, port):
    return psycopg.connect(host=host, port=port, user="postgres", dbname="postgres", autocommit=True)


# Each case: a full query with a %b (binary) placeholder cast to text (so both
# engines are compared on the same canonical representation), and the param.
CASES = [
    ("SELECT (%b::int4[])::text", [[1, 2, 3]]),
    ("SELECT (%b::int4[])::text", [[1, None, 3]]),
    ("SELECT (%b::int4[])::text", [[]]),
    ("SELECT (%b::text[])::text", [["a", "bb", "ccc"]]),
    ("SELECT (%b::int8[])::text", [[10, 9000000000]]),
    ("SELECT (%b::numeric[])::text", [["1.5", "-2.25", "300"]]),
    ("SELECT (%b::int4range)::text", [Range(1, 5, "[)")]),
    # An empty range has no subtype, so the client sends it untyped (OID 0);
    # the server must resolve int4range from the cast to decode the binary.
    ("SELECT (%b::int4range)::text", [Range(empty=True)]),
    ("SELECT (%b::int8range)::text", [Range(100, 200, "[]")]),
    ("SELECT (%b::int4multirange)::text", [Multirange([Range(1, 3, "[)"), Range(5, 7, "[)")])]),
]


def run_case(conn, sql, param):
    cur = conn.cursor()
    cur.execute(sql, param)
    return cur.fetchone()[0]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pg", type=int, required=True)
    ap.add_argument("--p3", type=int, required=True)
    ap.add_argument("--host", default="127.0.0.1")
    args = ap.parse_args()

    pg = connect(args.host, args.pg)
    p3 = connect(args.host, args.p3)

    fails = 0
    for sql, param in CASES:
        try:
            r_pg = run_case(pg, sql, param)
        except Exception as e:
            r_pg = f"ERROR:{type(e).__name__}"
        try:
            r_p3 = run_case(p3, sql, param)
        except Exception as e:
            r_p3 = f"ERROR:{type(e).__name__}"
        if r_pg == r_p3:
            print("ok:   %-34s -> %s" % (sql, r_pg))
        else:
            fails += 1
            print("DIFF: %-34s  pg=%r  p3=%r" % (sql, r_pg, r_p3))

    print("binary-param: %d check(s) failed" % fails)
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()

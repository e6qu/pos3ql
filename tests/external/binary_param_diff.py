#!/usr/bin/env python3
"""Extended-protocol binary composite differential: real PostgreSQL vs pos3ql.

psycopg's `%b` placeholder forces a parameter onto the wire in binary format, so
these bind composite parameters (arrays, ranges, multiranges) in binary and
check the value the server decodes and echoes back matches PostgreSQL exactly.
Scalar binary params were already exercised elsewhere; the point here is the
composite receive codec reached through the parameter path. The result cases
request binary DataRows too, covering the send side for ranges, multiranges,
and anonymous records.

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


# These contain no parameters: psycopg's `binary=True` asks each server for a
# binary Result format and decodes it through its PostgreSQL codecs. Comparing
# decoded values against real PostgreSQL catches a text payload mislabeled as
# binary just as reliably as comparing raw frames, while also exercising all
# subtype codecs clients actually use.
RESULT_CASES = [
    "SELECT '[1,5)'::int4range",
    "SELECT '[100,200]'::int8range",
    "SELECT '[1.25,300.00)'::numrange",
    "SELECT '[2024-01-02,2024-01-05)'::daterange",
    "SELECT '[\"2024-01-02 03:04:05\",\"2024-01-05 06:07:08\")'::tsrange",
    "SELECT '(,5)'::int4range",
    "SELECT 'empty'::int4range",
    "SELECT '{[1,3),[5,7)}'::int4multirange",
    "SELECT ROW(42::int4, NULL::text)",
    "SELECT ROW(state, positive, pair) FROM binary_result_rows",
    "SELECT ROW(binary_result_state_echo(state), binary_result_positive_echo(positive), "
    "binary_result_pair_echo(pair)) FROM binary_result_rows",
    "SELECT pg_options_to_table(ARRAY['fillfactor=80','flag'])",
    "SELECT pg_get_sequence_data(oid) FROM pg_class WHERE relname = 'binary_result_sequence'",
    "SELECT * FROM unnest(ARRAY[1::int4,2], ARRAY['a'::varchar(3)]) AS u(id,label) ORDER BY id",
    "SELECT * FROM ROWS FROM (generate_series(1,2), unnest(ARRAY['x'::varchar(2)])) "
    "WITH ORDINALITY AS r(series,label,ordinality) ORDER BY ordinality",
    "SELECT binary_result_values(4), generate_series(10,12)",
]


def run_case(conn, sql, param):
    cur = conn.cursor()
    cur.execute(sql, param)
    return cur.fetchone()[0]


def run_result_case(conn, sql):
    cur = conn.cursor()
    cur.execute(sql, binary=True)
    return cur.fetchall()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pg", type=int, required=True)
    ap.add_argument("--p3", type=int, required=True)
    ap.add_argument("--host", default="127.0.0.1")
    args = ap.parse_args()

    pg = connect(args.host, args.pg)
    p3 = connect(args.host, args.p3)

    setup = (
        "CREATE TYPE binary_result_state AS ENUM ('ready', 'blocked'); "
        "CREATE DOMAIN binary_result_positive AS integer CHECK (VALUE > 0); "
        "CREATE TYPE binary_result_pair AS (id integer, label text); "
        "CREATE TABLE binary_result_rows (state binary_result_state, positive binary_result_positive, "
        "pair binary_result_pair); "
        "INSERT INTO binary_result_rows VALUES ('ready', 7, ROW(1,'a')::binary_result_pair); "
        "CREATE FUNCTION binary_result_state_echo(binary_result_state) RETURNS binary_result_state "
        "LANGUAGE SQL AS 'SELECT $1'; "
        "CREATE FUNCTION binary_result_positive_echo(binary_result_positive) RETURNS binary_result_positive "
        "LANGUAGE SQL AS 'SELECT $1'; "
        "CREATE FUNCTION binary_result_pair_echo(binary_result_pair) RETURNS binary_result_pair "
        "LANGUAGE SQL AS 'SELECT $1'; "
        "CREATE FUNCTION binary_result_values(integer) RETURNS SETOF integer "
        "LANGUAGE SQL AS 'SELECT $1 UNION ALL SELECT $1 + 1'; "
        "CREATE SEQUENCE binary_result_sequence START WITH 7"
    )
    pg.execute(setup)
    p3.execute(setup)

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

    for sql in RESULT_CASES:
        pg_error = None
        try:
            r_pg = run_result_case(pg, sql)
        except Exception as e:
            r_pg = f"ERROR:{type(e).__name__}"
            pg_error = repr(e)
        p3_error = None
        try:
            r_p3 = run_result_case(p3, sql)
        except Exception as e:
            r_p3 = f"ERROR:{type(e).__name__}"
            p3_error = repr(e)
        if r_pg == r_p3:
            print("ok:   %-34s -> %s" % (sql, r_pg))
        else:
            fails += 1
            print("DIFF: %-34s  pg=%r  p3=%r" % (sql, r_pg, r_p3))
            if pg_error is not None:
                print("      PostgreSQL error: %s" % pg_error)
            if p3_error is not None:
                print("      pos3ql error: %s" % p3_error)

    print("binary-composite: %d check(s) failed" % fails)
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()

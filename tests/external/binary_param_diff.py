#!/usr/bin/env python3
"""Extended-protocol binary composite differential: real PostgreSQL vs pos3ql.

psycopg's `%b` placeholder forces a parameter onto the wire in binary format, so
these bind composite parameters (arrays, ranges, multiranges) in binary and
check the value the server decodes and echoes back matches PostgreSQL exactly.
Scalar binary params were already exercised elsewhere; the point here is the
composite receive codec reached through the parameter path. The result cases
request binary DataRows too, covering the send side for ranges, multiranges,
and anonymous records.

Every listed query is expected to succeed. Matching client errors are failures,
including when both servers reject the same result shape.

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

WIDE_MULTIRANGE = Multirange(
    [Range(index * 3, index * 3 + 1, "[)") for index in range(128)]
)
WIDE_MULTIRANGE_SQL = "'{%s}'::int4multirange" % ",".join(
    "[%d,%d)" % (index * 3, index * 3 + 1) for index in range(128)
)


def connect(host, port):
    return psycopg.connect(host=host, port=port, user="postgres", dbname="postgres", autocommit=True)


# Each case: a full query with a %b (binary) placeholder cast to text (so both
# engines are compared on the same canonical representation), and the param.
CASES = [
    ("SELECT reverse(%b::bytea)", [b"\x00\xffA"]),
    ("SELECT substring(%b::bytea FROM 2 FOR 2)", [b"\x00\x11\x22\x33"]),
    ("SELECT crc32(%b::bytea)", [b"123456789"]),
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
    ("SELECT (%b::int4multirange)::text", [WIDE_MULTIRANGE]),
]


# These contain no parameters: psycopg's `binary=True` asks each server for a
# binary Result format and decodes it through its PostgreSQL codecs. Comparing
# decoded values against real PostgreSQL catches a text payload mislabeled as
# binary just as reliably as comparing raw frames, while also exercising all
# subtype codecs clients actually use.
RESULT_CASES = [
    "SELECT overlay('\\x001122'::bytea PLACING '\\xaabb'::bytea FROM 2)",
    "SELECT set_bit(B'1001'::varbit, 1, 1)",
    "SELECT string_agg(value, delimiter ORDER BY ordinal) FROM "
    "(VALUES (2, '\\x62'::bytea, '\\x2d'::bytea), "
    "(1, '\\x6100'::bytea, '\\x2f'::bytea)) AS v(ordinal,value,delimiter)",
    "SELECT '[1,5)'::int4range",
    "SELECT '[100,200]'::int8range",
    "SELECT '[1.25,300.00)'::numrange",
    "SELECT '[2024-01-02,2024-01-05)'::daterange",
    "SELECT '[\"2024-01-02 03:04:05\",\"2024-01-05 06:07:08\")'::tsrange",
    "SELECT '(,5)'::int4range",
    "SELECT 'empty'::int4range",
    "SELECT '{[1,3),[5,7)}'::int4multirange",
    "SELECT " + WIDE_MULTIRANGE_SQL,
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
    "SELECT binary_pending_pl(42::int4), binary_pending_pl(9000000000::int8), "
    "binary_pending_pl(ARRAY[1,NULL,3])",
    "SELECT binary_pending_sql(42::int4), binary_pending_sql(9000000000::int8), "
    "binary_pending_sql(ARRAY[1,NULL,3])",
    "SELECT binary_pending_pl(state), binary_pending_pl(positive), binary_pending_pl(pair) "
    "FROM binary_result_rows",
    "SELECT binary_pending_sql(state), binary_pending_sql(positive), binary_pending_sql(pair) "
    "FROM binary_result_rows",
    "SELECT binary_pending_pl_rows(42::int4)",
    "SELECT * FROM binary_pending_pl_rows(9000000000::int8)",
    "SELECT binary_pending_sql_rows(ARRAY[1,NULL,3])",
    "SELECT * FROM binary_pending_sql_rows(42::int4)",
    "SELECT binary_pending_record(42::int4)",
    "SELECT (binary_pending_record(ARRAY[1,NULL,3])).*",
    "SELECT * FROM binary_pending_table(42::int4)",
    "SELECT * FROM binary_pending_table(ARRAY[1,NULL,3])",
    "SELECT (information_schema._pg_expandarray(ARRAY['a',NULL,'c'])).*",
    "SELECT (result.keys).x,(result.keys).n FROM "
    "(SELECT information_schema._pg_expandarray(ARRAY['x',NULL,'z']) AS keys) AS result "
    "ORDER BY (result.keys).n",
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
        "CREATE TABLE binary_pending_audit(value integer); "
        "CREATE FUNCTION binary_pending_pl(value anyelement) RETURNS anyelement "
        "LANGUAGE plpgsql AS $$BEGIN RETURN value; END$$; "
        "CREATE FUNCTION binary_pending_sql(value anyelement) RETURNS anyelement "
        "LANGUAGE sql VOLATILE AS 'INSERT INTO binary_pending_audit VALUES (1); SELECT $1'; "
        "CREATE FUNCTION binary_pending_pl_rows(value anyelement) RETURNS SETOF anyelement "
        "LANGUAGE plpgsql AS $$BEGIN RETURN NEXT value; END$$; "
        "CREATE FUNCTION binary_pending_sql_rows(value anyelement) RETURNS SETOF anyelement "
        "LANGUAGE sql VOLATILE AS 'INSERT INTO binary_pending_audit VALUES (2); "
        "SELECT $1 UNION ALL SELECT $1'; "
        "CREATE FUNCTION binary_pending_record(input_value anyelement,OUT value anyelement, "
        "OUT marker integer) LANGUAGE sql VOLATILE AS 'INSERT INTO binary_pending_audit "
        "VALUES (3); SELECT $1,7'; "
        "CREATE FUNCTION binary_pending_table(input_value anyelement) RETURNS TABLE "
        "(value anyelement,marker integer) LANGUAGE sql VOLATILE AS 'INSERT INTO "
        "binary_pending_audit VALUES (4); SELECT $1,8'; "
        "CREATE SEQUENCE binary_result_sequence START WITH 7"
    )
    pg.execute(setup)
    p3.execute(setup)

    fails = 0
    for sql, param in CASES:
        pg_error = None
        try:
            r_pg = run_case(pg, sql, param)
        except Exception as e:
            r_pg = f"ERROR:{type(e).__name__}"
            pg_error = repr(e)
        p3_error = None
        try:
            r_p3 = run_case(p3, sql, param)
        except Exception as e:
            r_p3 = f"ERROR:{type(e).__name__}"
            p3_error = repr(e)
        if pg_error is None and p3_error is None and r_pg == r_p3:
            print("ok:   %-34s -> %s" % (sql, r_pg))
        else:
            fails += 1
            print("DIFF: %-34s  pg=%r  p3=%r" % (sql, r_pg, r_p3))
            if pg_error is not None:
                print("      PostgreSQL error: %s" % pg_error)
            if p3_error is not None:
                print("      pos3ql error: %s" % p3_error)

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
        if pg_error is None and p3_error is None and r_pg == r_p3:
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

#!/usr/bin/env python3
"""PostgreSQL type fidelity matrix across Bind, portals, and binary COPY.

The focused raw-wire probes protect catalog-resolved domains, enums, records,
and malformed frames. This matrix protects the shared built-in type boundary:
PostgreSQL generates binary COPY values for every built-in value type with
unambiguous text input, pos3ql must accept them, emit the same bytes, and
expose the same text and binary query values through Bind and named portals.
Arrays, ranges, and multiranges are included in the matrix.

type_fidelity_diff.py --pg PORT --p3 PORT [--host HOST]
"""
import argparse
import sys

try:
    import psycopg
except ImportError:
    print("psycopg not installed", file=sys.stderr)
    sys.exit(2)


# Values deliberately include non-default values and NULL in a second row.
# Every expression has a concrete PostgreSQL type, so COPY checks the exact
# send/receive bytes rather than merely a text rendering.
STATIC_COLUMNS = [
    ("boolean_value", "boolean", "true"),
    ("small_integer_value", "smallint", "-32768"),
    ("integer_value", "integer", "123456"),
    ("big_integer_value", "bigint", "9000000000"),
    ("real_value", "real", "1.5"),
    ("double_precision_value", "double precision", "-2.25"),
    ("numeric_value", "numeric", "123456789.0123456789"),
    ("numeric_typmod_value", "numeric(12, 4)", "12345678.12345"),
    ("text_value", "text", "'héllo'"),
    ("varchar_value", "varchar(10)", "'abc'"),
    ("character_value", "char(5)", "'xy'"),
    ("name_value", "name", "'matrix_name'"),
    ("date_value", "date", "'2021-03-04'"),
    ("timestamp_value", "timestamp", "'2021-03-04 05:06:07.123456'"),
    ("timestamp_typmod_value", "timestamp(2)", "'2021-03-04 05:06:07.126789'"),
    ("timestamptz_value", "timestamptz", "'2021-03-04 05:06:07+02'"),
    ("timestamptz_typmod_value", "timestamptz(3)", "'2021-03-04 05:06:07.123789+02'"),
    ("time_value", "time", "'01:02:03.123456'"),
    ("time_typmod_value", "time(1)", "'01:02:03.159999'"),
    ("timetz_value", "timetz", "'08:09:10-05'"),
    ("timetz_typmod_value", "timetz(2)", "'08:09:10.125999-05'"),
    ("interval_value", "interval", "'1 year 2 mons 3 days 04:05:06.123456'"),
    ("interval_typmod_value", "interval(3)", "'1 year 2 mons 3 days 04:05:06.123789'"),
    ("json_value", "json", "'{\"a\":1}'"),
    ("jsonb_value", "jsonb", "'{\"b\": 2}'"),
    ("uuid_value", "uuid", "'00112233-4455-6677-8899-aabbccddeeff'"),
    ("bytea_value", "bytea", "'\\xcafe'"),
    ("bit_value", "bit(5)", "B'10110'"),
    ("varbit_value", "varbit", "B'101'"),
    ("varbit_typmod_value", "bit varying(5)", "B'10101'"),
    ("bit_array", "bit(5)[]", "ARRAY[B'10110', NULL, B'00111']::bit(5)[]"),
    ("varbit_array", "varbit[]", "ARRAY[B'1', NULL, B'00111']::varbit[]"),
    ("inet_value", "inet", "'192.0.2.1/24'"),
    ("cidr_value", "cidr", "'192.0.2.0/24'"),
    ("macaddr_value", "macaddr", "'08:00:2b:01:02:03'"),
    ("macaddr8_value", "macaddr8", "'08:00:2b:ff:fe:01:02:03'"),
    ("small_integer_array", "smallint[]", "ARRAY[-2, NULL, 3]::smallint[]"),
    ("integer_array", "integer[]", "ARRAY[-2, NULL, 3]"),
    ("big_integer_array", "bigint[]", "ARRAY[-2, NULL, 9000000000]::bigint[]"),
    ("real_array", "real[]", "ARRAY[1.5, NULL, -2.5]::real[]"),
    ("double_precision_array", "double precision[]", "ARRAY[1.5, NULL, -2.5]::float8[]"),
    ("numeric_array", "numeric[]", "ARRAY[1.25, NULL, -2.5]::numeric[]"),
    ("numeric_typmod_array", "numeric(6,2)[]", "ARRAY[1.234, NULL, -2.555]::numeric(6,2)[]"),
    ("boolean_array", "boolean[]", "ARRAY[true, NULL, false]"),
    ("text_array", "text[]", "ARRAY['a', NULL, 'bé']"),
    ("varchar_array", "varchar[]", "ARRAY['a', NULL, 'bé']::varchar[]"),
    ("character_array", "char(3)[]", "ARRAY['a', NULL, 'bé']::char(3)[]"),
    ("date_array", "date[]", "ARRAY['2021-01-01', NULL, '2021-01-03']::date[]"),
    ("timestamp_array", "timestamp[]", "ARRAY['2021-01-01 01:02:03', NULL, '2021-01-03']::timestamp[]"),
    ("timestamp_typmod_array", "timestamp(3)[]", "ARRAY['2021-01-01 01:02:03.123789', NULL, '2021-01-03 00:00:00.999999']::timestamp(3)[]"),
    ("timestamptz_array", "timestamptz[]", "ARRAY['2021-01-01 01:02:03+00', NULL, '2021-01-03 00:00:00+02']::timestamptz[]"),
    ("time_array", "time[]", "ARRAY['01:02:03', NULL, '04:05:06']::time[]"),
    ("timetz_array", "timetz[]", "ARRAY['01:02:03+00', NULL, '04:05:06-03']::timetz[]"),
    ("interval_array", "interval[]", "ARRAY['1 day', NULL, '-2 hours']::interval[]"),
    ("interval_typmod_array", "interval(2)[]", "ARRAY['1 day 00:00:00.123456', NULL, '-02:00:00.999999']::interval(2)[]"),
    ("uuid_array", "uuid[]", "ARRAY['00112233-4455-6677-8899-aabbccddeeff', NULL, 'ffffffff-ffff-ffff-ffff-ffffffffffff']::uuid[]"),
    ("bytea_array", "bytea[]", "ARRAY['\\x00', NULL, '\\xcafe']::bytea[]"),
    ("json_array", "json[]", "ARRAY['{\"a\":1}', NULL, '[2]']::json[]"),
    ("jsonb_array", "jsonb[]", "ARRAY['{\"a\":1}', NULL, '[2]']::jsonb[]"),
    ("inet_array", "inet[]", "ARRAY['192.0.2.1/24', NULL, '2001:db8::1/64']::inet[]"),
    ("cidr_array", "cidr[]", "ARRAY['192.0.2.0/24', NULL, '2001:db8::/64']::cidr[]"),
    ("macaddr_array", "macaddr[]", "ARRAY['08:00:2b:01:02:03', NULL, '08:00:2b:04:05:06']::macaddr[]"),
    ("macaddr8_array", "macaddr8[]", "ARRAY['08:00:2b:ff:fe:01:02:03', NULL, '08:00:2b:ff:fe:04:05:06']::macaddr8[]"),
    ("int4_range", "int4range", "'[1,5)'"),
    ("int8_range", "int8range", "'[100,200]'"),
    ("numeric_range", "numrange", "'[1.25,3.5)'"),
    ("date_range", "daterange", "'[2021-01-01,2021-01-03)'"),
    ("timestamp_range", "tsrange", "'[2021-01-01 01:02:03,2021-01-03 04:05:06)'"),
    ("timestamptz_range", "tstzrange", "'[2021-01-01 01:02:03+00,2021-01-03 04:05:06+00)'"),
    ("int4_multirange", "int4multirange", "'{[1,3),[5,7)}'"),
    ("int8_multirange", "int8multirange", "'{[1,3),[5,7)}'"),
    ("numeric_multirange", "nummultirange", "'{[1.25,3.5),[5,7)}'"),
    ("date_multirange", "datemultirange", "'{[2021-01-01,2021-01-03)}'"),
    ("timestamp_multirange", "tsmultirange", "'{[2021-01-01 01:02:03,2021-01-03 04:05:06)}'"),
    ("timestamptz_multirange", "tstzmultirange", "'{[2021-01-01 01:02:03+00,2021-01-03 04:05:06+00)}'"),
    ("object_identifier", "oid", "23"),
    ("type_reference", "regtype", "'integer'::regtype"),
    ("routine_reference", "regproc", "'version'::regproc"),
    ("routine_signature_reference", "regprocedure", "'version()'::regprocedure"),
    ("operator_signature_reference", "regoperator", "'+(integer,integer)'::regoperator"),
    ("namespace_reference", "regnamespace", "'public'::regnamespace"),
    ("role_reference", "regrole", "'postgres'::regrole"),
]


def connect(host, port):
    return psycopg.connect(host=host, port=port, user="postgres", dbname="postgres", autocommit=True)


def copy_out(conn, query):
    output = bytearray()
    with conn.cursor().copy(f"COPY ({query}) TO STDOUT (FORMAT binary)") as copier:
        for chunk in copier:
            output.extend(chunk)
    return bytes(output)


def copy_in(conn, table, data):
    with conn.cursor().copy(f"COPY {table} FROM STDIN (FORMAT binary)") as copier:
        copier.write(data)


def canonical_rows(conn, table, columns):
    fields = ", ".join(f"{name}::text" for name, _, _ in columns)
    cursor = conn.cursor()
    cursor.execute(f"SELECT {fields} FROM {table} ORDER BY marker")
    return cursor.fetchall()


def binary_rows(conn, table):
    cursor = conn.cursor(binary=True)
    cursor.execute(f"SELECT * FROM {table} ORDER BY marker")
    rows = cursor.fetchall()
    # psycopg exposes unknown catalog-defined types as bytes, which compare
    # directly.  Values compare semantically too: PostgreSQL's UTC decoder
    # uses datetime.timezone while pos3ql's valid TimeZone response uses
    # zoneinfo, and repr would mistake those equivalent values for a mismatch.
    return rows


def setup(conn, table, columns):
    definitions = ["marker integer NOT NULL"] + [f"{name} {type_name}" for name, type_name, _ in columns]
    values = ", ".join(value for _, _, value in columns)
    nulls = ", ".join("NULL" for _ in columns)
    cursor = conn.cursor()
    cursor.execute(f"DROP TABLE IF EXISTS {table}")
    cursor.execute(f"CREATE TABLE {table} ({', '.join(definitions)})")
    cursor.execute(f"INSERT INTO {table} VALUES (1, {values}), (2, {nulls})")


def text_bind_rows(conn, table, columns):
    cursor = conn.cursor()
    source_values = canonical_rows(conn, table, columns)[0]
    output = []
    for index, (_, type_name, _) in enumerate(columns):
        cursor.execute(f"SELECT %s::{type_name}::text", (source_values[index],))
        value = cursor.fetchone()[0]
        cursor.execute(f"SELECT %s::{type_name} IS NULL", (None,))
        output.append((value, cursor.fetchone()[0]))
    return output


def portal_rows(conn, table):
    # A named cursor uses Parse/Bind plus a named portal and repeated Execute.
    # Its declaration is text because PostgreSQL exposes that SQL cursor's
    # captured RowDescription through Describe Portal before FETCH chooses a
    # result format.
    with conn.transaction():
        cursor = conn.cursor(name="fidelity_matrix")
        cursor.execute(f"SELECT * FROM {table} ORDER BY marker")
        first = cursor.fetchmany(1)
        second = cursor.fetchmany(1)
        return first + second


def report(name, expected, actual):
    if expected == actual:
        print(f"ok:   {name}")
        return 0
    print(f"DIFF: {name}")
    print(f"  PostgreSQL: {expected!r}")
    print(f"  pos3ql:     {actual!r}")
    return 1


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--pg", type=int, required=True)
    parser.add_argument("--p3", type=int, required=True)
    parser.add_argument("--host", default="127.0.0.1")
    args = parser.parse_args()
    pg, p3 = connect(args.host, args.pg), connect(args.host, args.p3)
    failures = 0
    # Keep each relation under pos3ql's fixed column pool while preserving the
    # complete type matrix; protocol coverage is per typed column, not per
    # artificially wide row.
    columns_per_table = 16
    for group_index, first in enumerate(range(0, len(STATIC_COLUMNS), columns_per_table)):
        columns = STATIC_COLUMNS[first : first + columns_per_table]
        table = f"wire_fidelity_matrix_{group_index}"
        for connection in (pg, p3):
            setup(connection, table, columns)

        pg_copy = copy_out(pg, f"SELECT * FROM {table} ORDER BY marker")
        p3_copy = copy_out(p3, f"SELECT * FROM {table} ORDER BY marker")
        failures += report(f"binary COPY emits exact bytes for matrix group {group_index}", pg_copy, p3_copy)

        # PostgreSQL's bytes must be accepted by pos3ql and pos3ql's bytes by
        # PostgreSQL.  Delete first so the comparison proves receive semantics.
        for connection, data in ((pg, pg_copy), (p3, pg_copy)):
            connection.execute(f"DELETE FROM {table}")
            copy_in(connection, table, data)
        failures += report(
            f"PostgreSQL binary COPY input reconstructs group {group_index}",
            canonical_rows(pg, table, columns),
            canonical_rows(p3, table, columns),
        )

        p3_copy = copy_out(p3, f"SELECT * FROM {table} ORDER BY marker")
        pg.execute(f"DELETE FROM {table}")
        copy_in(pg, table, p3_copy)
        failures += report(
            f"pos3ql binary COPY output loads group {group_index} into PostgreSQL",
            canonical_rows(pg, table, columns),
            canonical_rows(p3, table, columns),
        )

        failures += report(
            f"binary Result decodes matrix group {group_index}", binary_rows(pg, table), binary_rows(p3, table)
        )
        failures += report(
            f"text Bind resolves matrix group {group_index}",
            text_bind_rows(pg, table, columns),
            text_bind_rows(p3, table, columns),
        )
        failures += report(
            f"named portal preserves matrix group {group_index}", portal_rows(pg, table), portal_rows(p3, table)
        )

    print(f"type-fidelity: {failures} check(s) failed; {len(STATIC_COLUMNS)} typed columns")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()

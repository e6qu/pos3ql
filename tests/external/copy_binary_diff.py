#!/usr/bin/env python3
"""COPY BINARY differential: real PostgreSQL vs pos3ql.

Binary COPY data is not line-oriented, so it cannot be fed through a psql `-f`
corpus the way the text/CSV corpora are. This drives both engines over the wire
with psycopg and checks three things against real PostgreSQL:

  1. COPY TO STDOUT (FORMAT binary) produces byte-identical output.
  2. Loading PostgreSQL's binary dump via COPY FROM STDIN (FORMAT binary)
     reconstructs identical rows on both engines.
  3. pos3ql's own binary dump loads back into PostgreSQL to the same rows.

Every supported scalar type (including catalog-stable identities) is checked
byte-for-byte. User-defined array elements have per-cluster OIDs, as they do
in PostgreSQL, so direct composite/enum arrays and domains over each are
compared after catalog-identity mapping and then loaded in both directions.
Exit 0 on full agreement, 1 on any diff.

  copy_binary_diff.py --pg PORT --p3 PORT [--host HOST]
"""
import argparse
import sys

try:
    import psycopg
except ImportError:
    print("psycopg not installed", file=sys.stderr)
    sys.exit(2)

DDL = """DROP TABLE IF EXISTS cb_composite;
DROP TABLE IF EXISTS cb;
DROP TABLE IF EXISTS cb_builtin_arrays;
DROP TABLE IF EXISTS cb_references;
DROP TABLE IF EXISTS cb_reference_relation;
DROP FUNCTION IF EXISTS cb_reference_routine(integer);
DROP DOMAIN IF EXISTS cb_mood_value;
DROP DOMAIN IF EXISTS cb_point_value;
DROP TYPE IF EXISTS cb_point;
DROP TYPE IF EXISTS cb_mood;
DROP DOMAIN IF EXISTS cb_positive;
CREATE TYPE cb_mood AS ENUM ('sad', 'ok', 'happy');
CREATE DOMAIN cb_positive AS int CHECK (VALUE > 0);
CREATE TYPE cb_point AS (x integer, y integer);
CREATE DOMAIN cb_point_value AS cb_point;
CREATE TABLE cb (
  i2 smallint, i4 int, i8 bigint, f4 real, f8 float8, n numeric, bo bool,
  t text, vc varchar(10), bp char(5), d date, ts timestamp, tz timestamptz,
  tm time, tt timetz, iv interval, j json, jb jsonb, u uuid, by bytea,
  ia int[], ta text[], r4 int4range, nr numrange, mr int4multirange,
  b5 bit(5), vb varbit, em cb_mood, dp cb_positive, ro regoperator,
  ba bool[], i2a smallint[], i8a bigint[], f4a real[], f8a float8[],
  na numeric[], namea name[], vca varchar[], bpa char[], da date[],
  tsa timestamp[], tza timestamptz[], tma time[], tta timetz[], iva interval[],
  ja json[], jba jsonb[], ua uuid[], bya bytea[], ineta inet[], cidra cidr[],
  maca macaddr[], mac8a macaddr8[], bita bit[], vbita varbit[])"""

INSERT = """INSERT INTO cb VALUES
  (32000, 123456, 9000000000, 1.5, 2.25, 123.456, true, 'héllo', 'abc', 'xy',
   '2021-03-04', '2021-03-04 05:06:07.89', '2021-03-04 05:06:07+02', '01:02:03.5',
   '08:09:10-05', '1 year 2 mons 3 days 04:05:06', '{"a":1}', '{"b": 2}',
   '00112233-4455-6677-8899-aabbccddeeff', '\\xcafe',
   '[2:4]={1,2,3}', '[3:4][5:6]={{a,bb},{c,d}}', '[1,5)', '[1.5,3.5]', '{[1,3),[5,7)}', B'10110', B'101',
   'happy', 5, '+(integer,integer)'::regoperator,
   ARRAY[true,false], ARRAY[1::smallint,-2::smallint], ARRAY[9000000000::bigint,-3::bigint],
   ARRAY[1.5::real,-2.25::real], ARRAY[2.25::float8,-3.5::float8], ARRAY[123.456::numeric,-0.001::numeric],
   ARRAY['named'::name], ARRAY['abc'::varchar], ARRAY['x'::char],
   ARRAY['2021-03-04'::date], ARRAY['2021-03-04 05:06:07.89'::timestamp],
   ARRAY['2021-03-04 05:06:07+02'::timestamptz], ARRAY['01:02:03.5'::time],
   ARRAY['08:09:10-05'::timetz], ARRAY['1 year 2 mons 3 days 04:05:06'::interval],
   ARRAY['{"a":1}'::json], ARRAY['{"b": 2}'::jsonb],
   ARRAY['00112233-4455-6677-8899-aabbccddeeff'::uuid], ARRAY['\\xcafe'::bytea],
   ARRAY['192.168.1.2/24'::inet], ARRAY['192.168.1.0/24'::cidr],
   ARRAY['08:00:2b:01:02:03'::macaddr], ARRAY['08:00:2b:01:02:03:04:05'::macaddr8],
   ARRAY[B'1'::bit], ARRAY[B'101'::varbit]),
  (-1, -123, -9000000000, -0.5, -1.25, -0.001, false, '', 'z', '', '2000-01-01',
   '1999-12-31 23:59:59', '2000-06-01 12:00:00+00', '00:00:00', '23:59:59+14',
   '-5 days', 'null', '[1,2,3]', 'ffffffff-ffff-ffff-ffff-ffffffffffff', '\\x',
   '{1,NULL,3}', '{}', 'empty', '(1.5,)', '{}', B'00000', B'', 'sad', 1,
   '+(integer,integer)'::regoperator,
   ARRAY[]::bool[], ARRAY[]::smallint[], ARRAY[]::bigint[], ARRAY[]::real[], ARRAY[]::float8[],
   ARRAY[]::numeric[], ARRAY[]::name[], ARRAY[]::varchar[], ARRAY[]::char[], ARRAY[]::date[],
   ARRAY[]::timestamp[], ARRAY[]::timestamptz[], ARRAY[]::time[], ARRAY[]::timetz[], ARRAY[]::interval[],
   ARRAY[]::json[], ARRAY[]::jsonb[], ARRAY[]::uuid[], ARRAY[]::bytea[], ARRAY[]::inet[], ARRAY[]::cidr[],
   ARRAY[]::macaddr[], ARRAY[]::macaddr8[], ARRAY[]::bit[], ARRAY[]::varbit[]),
  (NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,
   NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,
   NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL)"""

COMPOSITE_DDL = """DROP TABLE IF EXISTS cb_composite;
CREATE DOMAIN cb_mood_value AS cb_mood;
CREATE TABLE cb_composite (
  points cb_point[], marked_points cb_point_value[],
  moods cb_mood[], marked_moods cb_mood_value[])"""

COMPOSITE_INSERT = """INSERT INTO cb_composite VALUES
  (ARRAY[ROW(3,4)::cb_point], ARRAY[ROW(3,4)::cb_point]::cb_point_value[],
   ARRAY['sad'::cb_mood,'ok'::cb_mood], ARRAY['sad'::cb_mood]::cb_mood_value[]),
  (ARRAY[ROW(-1,8)::cb_point], ARRAY[ROW(-1,8)::cb_point]::cb_point_value[],
   ARRAY['happy'::cb_mood], ARRAY['happy'::cb_mood]::cb_mood_value[]),
  (NULL, NULL, NULL, NULL)"""

BUILTIN_ARRAY_DDL = """CREATE TABLE cb_builtin_arrays (
  int4_values int4range[], int8_values int8range[], numeric_values numrange[],
  date_values daterange[], timestamp_values tsrange[], timestamptz_values tstzrange[],
  int4_multi int4multirange[], int8_multi int8multirange[], numeric_multi nummultirange[],
  date_multi datemultirange[], timestamp_multi tsmultirange[], timestamptz_multi tstzmultirange[],
  oid_values oid[])"""

BUILTIN_ARRAY_INSERT = """INSERT INTO cb_builtin_arrays VALUES
  (ARRAY['[1,3)'::int4range], ARRAY['[1,3)'::int8range], ARRAY['[1.5,3.5)'::numrange],
   ARRAY['[2021-03-04,2021-03-06)'::daterange],
   ARRAY['[2021-03-04 05:06:07,2021-03-05 05:06:07)'::tsrange],
   ARRAY['[2021-03-04 05:06:07+00,2021-03-05 05:06:07+00)'::tstzrange],
   ARRAY['{[1,3)}'::int4multirange], ARRAY['{[1,3)}'::int8multirange],
   ARRAY['{[1.5,3.5)}'::nummultirange], ARRAY['{[2021-03-04,2021-03-06)}'::datemultirange],
   ARRAY['{[2021-03-04 05:06:07,2021-03-05 05:06:07)}'::tsmultirange],
   ARRAY['{[2021-03-04 05:06:07+00,2021-03-05 05:06:07+00)}'::tstzmultirange],
   ARRAY[1::oid, 4294967295::oid]),
  (ARRAY[]::int4range[], ARRAY[]::int8range[], ARRAY[]::numrange[], ARRAY[]::daterange[],
   ARRAY[]::tsrange[], ARRAY[]::tstzrange[], ARRAY[]::int4multirange[], ARRAY[]::int8multirange[],
   ARRAY[]::nummultirange[], ARRAY[]::datemultirange[], ARRAY[]::tsmultirange[], ARRAY[]::tstzmultirange[],
   ARRAY[]::oid[]),
  (NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL)"""

REFERENCE_DDL = """CREATE TABLE cb_reference_relation (id integer);
CREATE FUNCTION cb_reference_routine(value integer) RETURNS integer LANGUAGE SQL
  AS 'SELECT value';
CREATE TABLE cb_references (
  types regtype[], procedures regproc[], procedure_signatures regprocedure[],
  operators regoper[], operator_signatures regoperator[], relations regclass[],
  namespaces regnamespace[], roles regrole[])"""

REFERENCE_INSERT = """INSERT INTO cb_references VALUES (
  ARRAY['integer'::regtype],
  ARRAY['cb_reference_routine'::regproc],
  ARRAY['cb_reference_routine(integer)'::regprocedure],
  ARRAY[551::regoper], ARRAY['+(integer,integer)'::regoperator],
  ARRAY['cb_reference_relation'::regclass], ARRAY['public'::regnamespace],
  ARRAY[10::regrole])"""


def connect(host, port):
    return psycopg.connect(host=host, port=port, user="postgres", dbname="postgres", autocommit=True)


def dump(conn, table="cb"):
    out = b""
    with conn.cursor().copy("COPY %s TO STDOUT (FORMAT binary)" % table) as cp:
        for chunk in cp:
            out += bytes(chunk)
    return out


def dump_query(conn, query):
    out = b""
    with conn.cursor().copy("COPY (%s) TO STDOUT (FORMAT binary)" % query) as cp:
        for chunk in cp:
            out += bytes(chunk)
    return out


def load(conn, data, table="cb"):
    with conn.cursor().copy("COPY %s FROM STDIN (FORMAT binary)" % table) as cp:
        cp.write(data)


def rows(conn, table="cb"):
    cur = conn.cursor()
    if table == "cb":
        cur.execute("SELECT * FROM cb ORDER BY i4 NULLS LAST")
    elif table == "cb_composite":
        cur.execute(
            "SELECT points::text, marked_points::text, moods::text, marked_moods::text "
            "FROM cb_composite ORDER BY points::text NULLS LAST"
        )
    else:
        raise ValueError("unknown COPY differential fixture: %s" % table)
    return cur.fetchall()


def user_array_element_oids(conn):
    names = ("cb_point", "cb_point_value", "cb_mood", "cb_mood_value")
    rows = conn.execute(
        "SELECT typname, oid FROM pg_type WHERE typname = ANY(%s)", (list(names),)
    ).fetchall()
    by_name = {name: oid for name, oid in rows}
    return tuple(by_name[name] for name in names)


def reference_oids(conn):
    return (
        conn.execute("SELECT 'cb_reference_routine'::regproc::oid").fetchone()[0],
        conn.execute("SELECT 'cb_reference_relation'::regclass::oid").fetchone()[0],
    )


def remap_user_array_element_oids(data, source_oids, target_oids):
    """Map the user-defined array headers in the fixed composite fixture."""
    out = bytearray(data)
    if out[:19] != b"PGCOPY\n\xff\r\n\x00\x00\x00\x00\x00\x00\x00\x00\x00":
        raise AssertionError("invalid COPY binary signature")
    offset = 19
    while True:
        field_count = int.from_bytes(out[offset:offset + 2], "big", signed=True)
        offset += 2
        if field_count == -1:
            break
        if field_count != 4:
            raise AssertionError("expected four user-type array columns")
        for _ in range(field_count):
            field_length = int.from_bytes(out[offset:offset + 4], "big", signed=True)
            offset += 4
            if field_length == -1:
                continue
            if field_length < 12:
                raise AssertionError("invalid user-type array field")
            array_at = offset
            source_oid = int.from_bytes(out[array_at + 8:array_at + 12], "big", signed=True)
            try:
                target_oid = target_oids[source_oids.index(source_oid)]
            except ValueError as error:
                raise AssertionError("unexpected user-type array element OID") from error
            out[array_at + 8:array_at + 12] = target_oid.to_bytes(4, "big", signed=True)
            offset += field_length
    return bytes(out)


def remap_reference_array_values(data, source_oids, target_oids):
    """Map cluster-local routine and relation OIDs in the fixed reference fixture."""
    out = bytearray(data)
    if out[:19] != b"PGCOPY\n\xff\r\n\x00\x00\x00\x00\x00\x00\x00\x00\x00":
        raise AssertionError("invalid COPY binary signature")
    offset = 19
    while True:
        fields = int.from_bytes(out[offset:offset + 2], "big", signed=True)
        offset += 2
        if fields == -1:
            break
        if fields != 8:
            raise AssertionError("expected eight catalog-reference arrays")
        for _ in range(fields):
            length = int.from_bytes(out[offset:offset + 4], "big", signed=True)
            offset += 4
            if length == -1:
                continue
            end = offset + length
            dimensions = int.from_bytes(out[offset:offset + 4], "big", signed=True)
            if dimensions != 1:
                raise AssertionError("expected rank-one catalog-reference array")
            count = int.from_bytes(out[offset + 12:offset + 16], "big", signed=True)
            at = offset + 20
            for _ in range(count):
                value_length = int.from_bytes(out[at:at + 4], "big", signed=True)
                at += 4
                if value_length == 4:
                    value = int.from_bytes(out[at:at + 4], "big", signed=True)
                    for source, target in zip(source_oids, target_oids):
                        if value == source:
                            out[at:at + 4] = target.to_bytes(4, "big", signed=True)
                at += value_length
            if at != end:
                raise AssertionError("invalid catalog-reference array payload")
            offset = end
    return bytes(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pg", type=int, required=True)
    ap.add_argument("--p3", type=int, required=True)
    ap.add_argument("--host", default="127.0.0.1")
    args = ap.parse_args()

    pg = connect(args.host, args.pg)
    p3 = connect(args.host, args.p3)
    for c in (pg, p3):
        c.cursor().execute(DDL)
        c.cursor().execute(INSERT)

    fails = 0
    pg_dump, p3_dump = dump(pg), dump(p3)
    if pg_dump == p3_dump:
        print("ok: COPY TO binary byte-identical (%d bytes)" % len(pg_dump))
    else:
        fails += 1
        print("DIVERGENCE: COPY TO binary differs (pg=%d p3=%d)" % (len(pg_dump), len(p3_dump)))
        for i, (a, b) in enumerate(zip(pg_dump, p3_dump)):
            if a != b:
                print("  first byte diff at %d: pg=%02x p3=%02x" % (i, a, b))
                break

    pg_query_dump = dump_query(pg, "SELECT * FROM cb ORDER BY i4 NULLS LAST")
    p3_query_dump = dump_query(p3, "SELECT * FROM cb ORDER BY i4 NULLS LAST")
    if pg_query_dump == p3_query_dump:
        print("ok: COPY query binary byte-identical (%d bytes)" % len(pg_query_dump))
    else:
        fails += 1
        print(
            "DIVERGENCE: COPY query binary differs (pg=%d p3=%d)"
            % (len(pg_query_dump), len(p3_query_dump))
        )
        for i, (a, b) in enumerate(zip(pg_query_dump, p3_query_dump)):
            if a != b:
                print("  first byte diff at %d: pg=%02x p3=%02x" % (i, a, b))
                break

    # Load PostgreSQL's dump into both and compare the reconstructed rows.
    for c in (pg, p3):
        c.cursor().execute("DELETE FROM cb")
        load(c, pg_dump)
    r_pg, r_p3 = rows(pg), rows(p3)
    if r_pg == r_p3:
        print("ok: COPY FROM binary (PostgreSQL dump) rows identical")
    else:
        fails += 1
        print("DIVERGENCE: COPY FROM binary rows differ")
        for a, b in zip(r_pg, r_p3):
            for j, (x, y) in enumerate(zip(a, b)):
                if x != y:
                    print("  col %d: pg=%r p3=%r" % (j, x, y))

    # pos3ql's own dump must load back into PostgreSQL to the same rows.
    pg.cursor().execute("DELETE FROM cb")
    load(pg, p3_dump)
    if rows(pg) == r_p3:
        print("ok: pos3ql binary dump loads into PostgreSQL")
    else:
        fails += 1
        print("DIVERGENCE: pos3ql binary dump does not round-trip through PostgreSQL")

    # User-defined type OIDs are allocated independently per PostgreSQL
    # cluster. Their array headers are catalog identity, not portable value data.
    for c in (pg, p3):
        c.cursor().execute(COMPOSITE_DDL)
        c.cursor().execute(COMPOSITE_INSERT)
    pg_composite_oids, p3_composite_oids = user_array_element_oids(pg), user_array_element_oids(p3)
    pg_composite_dump = dump(pg, "cb_composite")
    p3_composite_dump = dump(p3, "cb_composite")
    pg_body = remap_user_array_element_oids(pg_composite_dump, pg_composite_oids, (0, 0, 0, 0))
    p3_body = remap_user_array_element_oids(p3_composite_dump, p3_composite_oids, (0, 0, 0, 0))
    if pg_body == p3_body:
        print("ok: user-type array COPY bodies match after catalog OID mapping")
    else:
        fails += 1
        print("DIVERGENCE: user-type array COPY bodies differ")

    # A client transferring a user-defined binary value maps its catalog OID.
    # This checks that direct and domain identities retain their composite
    # records or enum labels rather than becoming text-shaped stand-ins.
    for c, data, source_oid, target_oid in (
        (p3, pg_composite_dump, pg_composite_oids, p3_composite_oids),
        (pg, p3_composite_dump, p3_composite_oids, pg_composite_oids),
    ):
        c.cursor().execute("DELETE FROM cb_composite")
        load(c, remap_user_array_element_oids(data, source_oid, target_oid), "cb_composite")
    if rows(pg, "cb_composite") == rows(p3, "cb_composite"):
        print("ok: catalog-mapped user-type arrays load in both directions")
    else:
        fails += 1
        print("DIVERGENCE: catalog-mapped user-type array rows differ")

    # These built-in array OIDs are portable, so their dumps must agree
    # byte-for-byte and cross-load unchanged.
    for c in (pg, p3):
        c.cursor().execute(BUILTIN_ARRAY_DDL)
        c.cursor().execute(BUILTIN_ARRAY_INSERT)
    pg_range_dump = dump(pg, "cb_builtin_arrays")
    p3_range_dump = dump(p3, "cb_builtin_arrays")
    if pg_range_dump == p3_range_dump:
        print("ok: built-in range and oid array COPY bodies are byte-identical")
    else:
        fails += 1
        print("DIVERGENCE: built-in range and oid array COPY bodies differ")
    for c, data in ((pg, pg_range_dump), (p3, pg_range_dump)):
        c.cursor().execute("DELETE FROM cb_builtin_arrays")
        load(c, data, "cb_builtin_arrays")
    range_text = "SELECT " + ", ".join(
        name + "::text"
        for name in (
            "int4_values", "int8_values", "numeric_values", "date_values",
            "timestamp_values", "timestamptz_values", "int4_multi", "int8_multi",
            "numeric_multi", "date_multi", "timestamp_multi", "timestamptz_multi",
            "oid_values",
        )
    ) + ' FROM cb_builtin_arrays ORDER BY int4_values::text COLLATE "C" NULLS LAST'
    pg_range_rows = pg.execute(range_text).fetchall()
    p3_range_rows = p3.execute(range_text).fetchall()
    if pg_range_rows == p3_range_rows:
        print("ok: PostgreSQL built-in array COPY input reconstructs identical rows")
    else:
        fails += 1
        print("DIVERGENCE: PostgreSQL built-in array COPY input rows differ")
        print("  PostgreSQL: %r" % (pg_range_rows,))
        print("  pos3ql:     %r" % (p3_range_rows,))
    pg.cursor().execute("DELETE FROM cb_builtin_arrays")
    load(pg, p3_range_dump, "cb_builtin_arrays")
    if pg.execute(range_text).fetchall() == p3_range_rows:
        print("ok: pos3ql built-in array binary dump loads into PostgreSQL")
    else:
        fails += 1
        print("DIVERGENCE: pos3ql built-in array dump does not round-trip through PostgreSQL")
        print("  PostgreSQL: %r" % (pg.execute(range_text).fetchall(),))
        print("  pos3ql:     %r" % (p3_range_rows,))

    for c in (pg, p3):
        c.cursor().execute(REFERENCE_DDL)
        c.cursor().execute(REFERENCE_INSERT)
    pg_reference_oids, p3_reference_oids = reference_oids(pg), reference_oids(p3)
    pg_reference_dump = dump(pg, "cb_references")
    p3_reference_dump = dump(p3, "cb_references")
    if remap_reference_array_values(pg_reference_dump, pg_reference_oids, p3_reference_oids) == p3_reference_dump:
        print("ok: catalog-reference array COPY bodies match after OID mapping")
    else:
        fails += 1
        print("DIVERGENCE: catalog-reference array COPY bodies differ")
    for c, data, source_oids, target_oids in (
        (p3, pg_reference_dump, pg_reference_oids, p3_reference_oids),
        (pg, p3_reference_dump, p3_reference_oids, pg_reference_oids),
    ):
        c.cursor().execute("DELETE FROM cb_references")
        load(c, remap_reference_array_values(data, source_oids, target_oids), "cb_references")
    pg_reference_rows = pg.execute("SELECT * FROM cb_references").fetchall()
    p3_reference_rows = p3.execute("SELECT * FROM cb_references").fetchall()
    if pg_reference_rows == p3_reference_rows:
        print("ok: catalog-mapped reference-array COPY loads in both directions")
    else:
        fails += 1
        print("DIVERGENCE: catalog-mapped reference-array COPY rows differ")
        print("  PostgreSQL: %r" % (pg_reference_rows,))
        print("  pos3ql:     %r" % (p3_reference_rows,))

    print("copy-binary: %d check(s) failed" % fails)
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()

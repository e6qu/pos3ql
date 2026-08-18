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
byte-for-byte. Composite-domain arrays have per-cluster element OIDs, as they
do in PostgreSQL, so their record bodies are compared after catalog-identity
mapping and then loaded in both directions. Exit 0 on full agreement, 1 on
any diff.

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
  b5 bit(5), vb varbit, em cb_mood, dp cb_positive, ro regoperator)"""

INSERT = """INSERT INTO cb VALUES
  (32000, 123456, 9000000000, 1.5, 2.25, 123.456, true, 'héllo', 'abc', 'xy',
   '2021-03-04', '2021-03-04 05:06:07.89', '2021-03-04 05:06:07+02', '01:02:03.5',
   '08:09:10-05', '1 year 2 mons 3 days 04:05:06', '{"a":1}', '{"b": 2}',
   '00112233-4455-6677-8899-aabbccddeeff', '\\xcafe',
   '{1,2,3}', '{a,bb}', '[1,5)', '[1.5,3.5]', '{[1,3),[5,7)}', B'10110', B'101',
   'happy', 5, '+(integer,integer)'::regoperator),
  (-1, -123, -9000000000, -0.5, -1.25, -0.001, false, '', 'z', '', '2000-01-01',
   '1999-12-31 23:59:59', '2000-06-01 12:00:00+00', '00:00:00', '23:59:59+14',
   '-5 days', 'null', '[1,2,3]', 'ffffffff-ffff-ffff-ffff-ffffffffffff', '\\x',
   '{1,NULL,3}', '{}', 'empty', '(1.5,)', '{}', B'00000', B'', 'sad', 1,
   '+(integer,integer)'::regoperator),
  (NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,
   NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL)"""

COMPOSITE_DDL = """DROP TABLE IF EXISTS cb_composite;
CREATE TABLE cb_composite (points cb_point_value[])"""

COMPOSITE_INSERT = """INSERT INTO cb_composite VALUES
  (ARRAY[ROW(3,4)::cb_point]::cb_point_value[]),
  (ARRAY[ROW(-1,8)::cb_point]::cb_point_value[]),
  (NULL)"""


def connect(host, port):
    return psycopg.connect(host=host, port=port, user="postgres", dbname="postgres", autocommit=True)


def dump(conn, table="cb"):
    out = b""
    with conn.cursor().copy("COPY %s TO STDOUT (FORMAT binary)" % table) as cp:
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
        cur.execute("SELECT points::text FROM cb_composite ORDER BY points::text NULLS LAST")
    else:
        raise ValueError("unknown COPY differential fixture: %s" % table)
    return cur.fetchall()


def composite_domain_oid(conn):
    return conn.execute(
        "SELECT oid FROM pg_type WHERE typname = 'cb_point_value'"
    ).fetchone()[0]


def remap_composite_array_domain_oid(data, source_oid, target_oid):
    """Map every non-NULL array header in this fixed one-column fixture."""
    out = bytearray(data)
    if out[:19] != b"PGCOPY\n\xff\r\n\x00\x00\x00\x00\x00\x00\x00\x00\x00":
        raise AssertionError("invalid COPY binary signature")
    offset = 19
    while True:
        field_count = int.from_bytes(out[offset:offset + 2], "big", signed=True)
        offset += 2
        if field_count == -1:
            break
        if field_count != 1:
            raise AssertionError("expected one composite-array column")
        field_length = int.from_bytes(out[offset:offset + 4], "big", signed=True)
        offset += 4
        if field_length == -1:
            continue
        if field_length < 12:
            raise AssertionError("invalid composite-domain array field")
        array_at = offset
        if int.from_bytes(out[array_at + 8:array_at + 12], "big", signed=True) != source_oid:
            raise AssertionError("unexpected composite-domain array element OID")
        out[array_at + 8:array_at + 12] = target_oid.to_bytes(4, "big", signed=True)
        offset += field_length
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
    # cluster. Their array header is catalog identity, not portable value data.
    for c in (pg, p3):
        c.cursor().execute(COMPOSITE_DDL)
        c.cursor().execute(COMPOSITE_INSERT)
    pg_composite_oid, p3_composite_oid = composite_domain_oid(pg), composite_domain_oid(p3)
    pg_composite_dump = dump(pg, "cb_composite")
    p3_composite_dump = dump(p3, "cb_composite")
    pg_body = remap_composite_array_domain_oid(pg_composite_dump, pg_composite_oid, 0)
    p3_body = remap_composite_array_domain_oid(p3_composite_dump, p3_composite_oid, 0)
    if pg_body == p3_body:
        print("ok: composite-domain array COPY bodies match after catalog OID mapping")
    else:
        fails += 1
        print("DIVERGENCE: composite-domain array COPY bodies differ")

    # A client transferring a user-defined binary value maps its catalog OID.
    # This checks that the remaining element is a composite record in both
    # directions rather than a text-shaped stand-in.
    for c, data, source_oid, target_oid in (
        (p3, pg_composite_dump, pg_composite_oid, p3_composite_oid),
        (pg, p3_composite_dump, p3_composite_oid, pg_composite_oid),
    ):
        c.cursor().execute("DELETE FROM cb_composite")
        load(c, remap_composite_array_domain_oid(data, source_oid, target_oid), "cb_composite")
    if rows(pg, "cb_composite") == rows(p3, "cb_composite"):
        print("ok: catalog-mapped composite-domain array COPY loads in both directions")
    else:
        fails += 1
        print("DIVERGENCE: catalog-mapped composite-domain array rows differ")

    print("copy-binary: %d check(s) failed" % fails)
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()

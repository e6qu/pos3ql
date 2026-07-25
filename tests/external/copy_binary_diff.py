#!/usr/bin/env python3
"""COPY BINARY differential: real PostgreSQL vs pos3ql.

Binary COPY data is not line-oriented, so it cannot be fed through a psql `-f`
corpus the way the text/CSV corpora are. This drives both engines over the wire
with psycopg and checks three things against real PostgreSQL:

  1. COPY TO STDOUT (FORMAT binary) produces byte-identical output.
  2. Loading PostgreSQL's binary dump via COPY FROM STDIN (FORMAT binary)
     reconstructs identical rows on both engines.
  3. pos3ql's own binary dump loads back into PostgreSQL to the same rows.

Every supported column type (the scalar / numeric / temporal / uuid / bytea /
json tower, with NULLs) is exercised. Exit 0 on full agreement, 1 on any diff.

  copy_binary_diff.py --pg PORT --p3 PORT [--host HOST]
"""
import argparse
import sys

try:
    import psycopg
except ImportError:
    print("psycopg not installed", file=sys.stderr)
    sys.exit(2)

DDL = """DROP TABLE IF EXISTS cb;
CREATE TABLE cb (
  i2 smallint, i4 int, i8 bigint, f4 real, f8 float8, n numeric, bo bool,
  t text, vc varchar(10), bp char(5), d date, ts timestamp, tz timestamptz,
  tm time, tt timetz, iv interval, j json, jb jsonb, u uuid, by bytea,
  ia int[], ta text[], r4 int4range, nr numrange, mr int4multirange,
  b5 bit(5), vb varbit)"""

INSERT = """INSERT INTO cb VALUES
  (32000, 123456, 9000000000, 1.5, 2.25, 123.456, true, 'héllo', 'abc', 'xy',
   '2021-03-04', '2021-03-04 05:06:07.89', '2021-03-04 05:06:07+02', '01:02:03.5',
   '08:09:10-05', '1 year 2 mons 3 days 04:05:06', '{"a":1}', '{"b": 2}',
   '00112233-4455-6677-8899-aabbccddeeff', '\\xcafe',
   '{1,2,3}', '{a,bb}', '[1,5)', '[1.5,3.5]', '{[1,3),[5,7)}', B'10110', B'101'),
  (-1, -123, -9000000000, -0.5, -1.25, -0.001, false, '', 'z', '', '2000-01-01',
   '1999-12-31 23:59:59', '2000-06-01 12:00:00+00', '00:00:00', '23:59:59+14',
   '-5 days', 'null', '[1,2,3]', 'ffffffff-ffff-ffff-ffff-ffffffffffff', '\\x',
   '{1,NULL,3}', '{}', 'empty', '(1.5,)', '{}', B'00000', B''),
  (NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,
   NULL,NULL,NULL,NULL,NULL,NULL,NULL)"""


def connect(host, port):
    return psycopg.connect(host=host, port=port, user="postgres", dbname="postgres", autocommit=True)


def dump(conn):
    out = b""
    with conn.cursor().copy("COPY cb TO STDOUT (FORMAT binary)") as cp:
        for chunk in cp:
            out += bytes(chunk)
    return out


def load(conn, data):
    with conn.cursor().copy("COPY cb FROM STDIN (FORMAT binary)") as cp:
        cp.write(data)


def rows(conn):
    cur = conn.cursor()
    cur.execute("SELECT * FROM cb ORDER BY i4 NULLS LAST")
    return cur.fetchall()


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

    print("copy-binary: %d check(s) failed" % fails)
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()

-- Grouping keys match semantically (a and t.a are one key), aggregates sort
-- grouped output, stars expand in grouped selects; the name type and regtype
-- resolution. Each probe diverged before its fix.
CREATE TABLE gk (a int, b int, c int);
INSERT INTO gk VALUES (1,1,10),(1,2,20),(2,1,30);

-- One key, two spellings.
SELECT t.a FROM gk t GROUP BY a ORDER BY 1;
SELECT a FROM gk GROUP BY gk.a ORDER BY 1;
SELECT a FROM gk t GROUP BY t.a ORDER BY 1;
SELECT gk.a + 1 FROM gk GROUP BY a ORDER BY 1;
SELECT grouping(t.a), t.a FROM gk t GROUP BY a ORDER BY 2;

-- Stars expand into grouped columns.
SELECT * FROM gk GROUP BY a, b, c ORDER BY a, b;
SELECT t.* FROM gk t GROUP BY a, b, c ORDER BY a, b;
SELECT * FROM gk GROUP BY b, a, c ORDER BY a, b;

-- Aggregates in ORDER BY.
SELECT a FROM gk GROUP BY a ORDER BY count(*), a;
SELECT a FROM gk GROUP BY a ORDER BY sum(c) DESC, a;
SELECT a, count(*) FROM gk GROUP BY a ORDER BY count(*) DESC, a;
SELECT 1 FROM gk ORDER BY count(*);
SELECT b FROM gk GROUP BY b ORDER BY max(c) - min(c), b;

-- The grouping rule reaches every clause with PostgreSQL's error.
SELECT a FROM gk ORDER BY count(*);
SELECT b FROM gk GROUP BY a;
SELECT a FROM gk GROUP BY a HAVING b > 1;
SELECT a FROM gk GROUP BY a ORDER BY b;
SELECT * FROM gk GROUP BY a, b;

-- name: the identifier type.
SELECT pg_typeof(current_user), pg_typeof(session_user), pg_typeof(current_schema);
SELECT pg_typeof('x'::name), 'x'::name, length('x'::name);
SELECT octet_length(repeat('x', 100)::name::text);
SELECT 'x'::name = 'x', 'x'::name || 'y';

-- regtype: resolution from names and OIDs, canonical SQL rendering.
SELECT 'int4'::regtype, 'integer'::regtype, 'varchar(5)'::regtype;
SELECT 'timestamp'::regtype, 'TIMESTAMPTZ'::regtype, 'timestamp without time zone'::regtype;
SELECT 'double precision'::regtype, 'float'::regtype, 'char'::regtype;
SELECT 'name'::regtype, 'oid'::regtype, 'regtype'::regtype;
SELECT 23::regtype, 9999::regtype, 0::regtype;
SELECT 'int4'::regtype = 'integer'::regtype;
SELECT 'nosuch'::regtype;
SELECT 'serial'::regtype;

DROP TABLE gk;

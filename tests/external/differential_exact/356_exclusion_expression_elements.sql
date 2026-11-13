-- Exclusion constraint elements may be function calls or parenthesized
-- expressions: catalogs, conflict details, copies and column changes.
CREATE FUNCTION ee356_detail(statement text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  detail text;
BEGIN
  EXECUTE statement;
  RETURN 'ok';
EXCEPTION WHEN exclusion_violation THEN
  GET STACKED DIAGNOSTICS detail = PG_EXCEPTION_DETAIL;
  RETURN detail;
END
$$;
CREATE TABLE ee356 (a int, b int, r int4range, active boolean,
  EXCLUDE USING gist (int4range(a, b) WITH &&),
  EXCLUDE USING gist ((r * '[1,10)'::int4range) WITH &&,
                      int4range(a, b, '[]') WITH -|-) WHERE (active));
ALTER TABLE ee356 ADD EXCLUDE USING gist ((r) WITH &&, r WITH =, int4range(a, b) WITH &&);
\d ee356
SELECT conname, pg_get_constraintdef(oid), pg_get_constraintdef(oid, true),
       conkey, conexclop
  FROM pg_constraint WHERE conrelid = 'ee356'::regclass ORDER BY conname;
SELECT indexrelid::regclass, indkey, indnatts, indexprs IS NOT NULL,
       pg_get_indexdef(indexrelid), pg_get_indexdef(indexrelid, 1, true)
  FROM pg_index WHERE indrelid = 'ee356'::regclass ORDER BY 1;
SELECT objid::regclass, refobjsubid, deptype FROM pg_depend
 WHERE refobjid = 'ee356'::regclass AND classid = 'pg_class'::regclass
   AND deptype = 'a' ORDER BY 1, 2;
SELECT attrelid::regclass, attname, atttypid::regtype FROM pg_attribute
 WHERE attrelid IN (SELECT indexrelid FROM pg_index WHERE indrelid = 'ee356'::regclass)
 ORDER BY 1, attnum;
INSERT INTO ee356 VALUES (1, 3, NULL, false);
SELECT ee356_detail($$INSERT INTO ee356 VALUES (2, 4, NULL, false)$$);
SELECT ee356_detail($$INSERT INTO ee356 VALUES (NULL, 9, NULL, false)$$);
INSERT INTO ee356 VALUES (5, 5, NULL, false), (5, 5, NULL, false);
INSERT INTO ee356 VALUES (10, 12, '[1,4)', true);
SELECT ee356_detail($$INSERT INTO ee356 VALUES (13, 14, '[3,5)', true)$$);
INSERT INTO ee356 VALUES (13, 14, '[3,5)', false);
SELECT ee356_detail($$UPDATE ee356 SET b = 4 WHERE a = 1$$);
SELECT ee356_detail($$ALTER TABLE ee356 ADD EXCLUDE USING gist (int4range(a, a, '[]') WITH &&)$$);
ALTER TABLE ee356 ADD EXCLUDE USING gist (int4range(z, 2) WITH &&);
ALTER TABLE ee356 ADD EXCLUDE USING gist (now() WITH =);
ALTER TABLE ee356 ADD EXCLUDE USING gist (r + r WITH &&);
ALTER TABLE ee356 ADD EXCLUDE USING gist ((r) DESC WITH &&);
ALTER TABLE ee356 ADD EXCLUDE USING gist ((r) COLLATE "C" WITH &&);
ALTER TABLE ee356 ADD EXCLUDE USING gist (lower(r) WITH =);
-- Copies keep the expressions; renames, retyping and drops follow them.
CREATE TABLE ee356_like (LIKE ee356 INCLUDING ALL);
\d ee356_like
CREATE TABLE ee356_child (c int) INHERITS (ee356);
\d ee356_child
ALTER TABLE ee356 RENAME COLUMN a TO start_at;
\d ee356
ALTER TABLE ee356 ALTER COLUMN b TYPE bigint;
ALTER TABLE ee356 ALTER COLUMN b TYPE smallint;
SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint
 WHERE conrelid = 'ee356'::regclass ORDER BY conname;
SELECT ee356_detail($$INSERT INTO ee356 VALUES (2, 4, NULL, false)$$);
DROP TABLE ee356_child;
ALTER TABLE ee356 DROP COLUMN b;
ALTER TABLE ee356 DROP COLUMN b CASCADE;
\d ee356
-- Deferred checks evaluate the expressions at commit.
CREATE TABLE ee356_deferred (s timestamp, e timestamp,
  CONSTRAINT ee356_deferred_slot EXCLUDE USING gist (tsrange(s, e) WITH &&)
    DEFERRABLE INITIALLY DEFERRED);
BEGIN;
INSERT INTO ee356_deferred VALUES ('2024-01-01 10:00', '2024-01-01 11:00');
INSERT INTO ee356_deferred VALUES ('2024-01-01 10:30', '2024-01-01 12:00');
UPDATE ee356_deferred SET e = '2024-01-01 10:30' WHERE s = '2024-01-01 10:00';
COMMIT;
BEGIN;
INSERT INTO ee356_deferred VALUES ('2024-01-01 10:15', '2024-01-01 10:20');
COMMIT;
SELECT * FROM ee356_deferred ORDER BY s;
-- Partitions clone the expression elements; the partition key must be an
-- equality column element.
CREATE TABLE ee356_parts (a int, b int, p int4range) PARTITION BY LIST (p);
ALTER TABLE ee356_parts ADD EXCLUDE USING gist (int4range(a, b) WITH &&);
ALTER TABLE ee356_parts ADD EXCLUDE USING gist (p WITH &&);
ALTER TABLE ee356_parts ADD EXCLUDE USING gist ((p) WITH =, int4range(a, b) WITH &&);
CREATE TABLE ee356_part1 PARTITION OF ee356_parts FOR VALUES IN ('[1,2)');
CREATE TABLE ee356_part2 (b int, a int, p int4range);
ALTER TABLE ee356_parts ATTACH PARTITION ee356_part2 FOR VALUES IN ('[2,3)');
SELECT conrelid::regclass, conname, pg_get_constraintdef(oid), conparentid <> 0
  FROM pg_constraint WHERE conrelid::regclass::text LIKE 'ee356_part%'
 ORDER BY 1, 2;
INSERT INTO ee356_parts VALUES (1, 5, '[2,3)');
SELECT ee356_detail($$INSERT INTO ee356_parts VALUES (3, 7, '[2,3)')$$);
INSERT INTO ee356_parts VALUES (3, 7, '[1,2)');
DROP TABLE ee356, ee356_like, ee356_deferred, ee356_parts;
DROP FUNCTION ee356_detail(text);

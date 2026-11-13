-- An exclusion constraint's partial predicate prints through its index the
-- way pg_dump reads it.
CREATE TABLE ep354 (id int, slot int4range, active boolean,
  CONSTRAINT ep354_slot EXCLUDE USING gist (slot WITH &&) WHERE (active));
SELECT pg_get_indexdef(i.indexrelid), pg_get_constraintdef(c.oid),
       pg_get_expr(i.indpred, i.indrelid), i.indexprs IS NULL
  FROM pg_index i JOIN pg_constraint c ON c.conindid = i.indexrelid
 WHERE i.indrelid = 'ep354'::regclass;
INSERT INTO ep354 VALUES (1, '[1,4)', true), (2, '[2,5)', false);
INSERT INTO ep354 VALUES (3, '[3,6)', true);
-- A catalog lookup keyed by a constant cast probes the catalog rows once.
SELECT attname, atttypid::regtype FROM pg_attribute
 WHERE attrelid = 'ep354'::regclass AND attnum > 0 ORDER BY attnum;
DROP TABLE ep354;

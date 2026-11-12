-- IS [NOT] DISTINCT FROM and IS [NOT] TRUE/FALSE deparse as PostgreSQL's
-- DistinctExpr (negated for IS NOT DISTINCT FROM) and BooleanTest nodes.
CREATE TABLE dp_t(a integer, b integer, c boolean, CHECK (a IS DISTINCT FROM b), CHECK (a IS NOT DISTINCT FROM 1), CHECK (c IS TRUE), CHECK (c IS NOT FALSE), CHECK (c IS NOT TRUE OR a > 0), CHECK ((a + 1) IS DISTINCT FROM (b * 2)), CHECK (NOT c IS FALSE));
SELECT conname, pg_get_constraintdef(oid), pg_get_constraintdef(oid, true) FROM pg_constraint WHERE conrelid = 'dp_t'::regclass ORDER BY conname;
CREATE TABLE dp_d(a integer, b boolean DEFAULT (1 IS DISTINCT FROM 2), c boolean DEFAULT (NULL IS NOT TRUE));
SELECT attname, pg_get_expr(adbin, adrelid) FROM pg_attrdef JOIN pg_attribute ON attrelid = adrelid AND attnum = adnum WHERE adrelid = 'dp_d'::regclass ORDER BY adnum;
CREATE INDEX dp_t_idx ON dp_t (a) WHERE a IS DISTINCT FROM 0;
SELECT pg_get_indexdef('dp_t_idx'::regclass);
INSERT INTO dp_t VALUES (1, 2, true);
SELECT a IS DISTINCT FROM b, a IS NOT DISTINCT FROM 1, c IS NOT TRUE FROM dp_t;
DROP TABLE dp_t;
DROP TABLE dp_d;

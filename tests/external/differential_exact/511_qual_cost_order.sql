-- A condition's conjuncts run in PostgreSQL's cost order
-- (order_qual_clauses): a routine costs its declared COST (100 unless
-- written otherwise) times an operator, so `id < 3` runs before an
-- expensive call written first, in queries, data changes and EXPLAIN.
CREATE TABLE c511 (id int, a int);
INSERT INTO c511 SELECT g, g * 10 FROM generate_series(1, 6) g;
CREATE FUNCTION c511_f(v int) RETURNS bool LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE 'f %', v; RETURN true; END $$;
CREATE FUNCTION c511_g(v int) RETURNS bool LANGUAGE plpgsql COST 1 AS $$
BEGIN RAISE NOTICE 'g %', v; RETURN true; END $$;
SELECT id FROM c511 WHERE c511_f(id) AND id < 3;
SELECT id FROM c511 WHERE c511_f(id) AND c511_g(id) AND id < 3;
SELECT id FROM c511 WHERE (c511_f(id) OR id < 0) AND id < 2;
SELECT id FROM c511 WHERE c511_f(id) AND id IN (1, 2);
SELECT id FROM c511 WHERE c511_f(id) AND a + 1 > 50 AND id < 6;
EXPLAIN (COSTS OFF) SELECT id FROM c511 WHERE c511_f(id) AND c511_g(id) AND id < 3;
UPDATE c511 SET a = a WHERE c511_f(id) AND id < 2;
EXPLAIN (COSTS OFF) UPDATE c511 SET a = a WHERE c511_f(id) AND id < 2;
DELETE FROM c511 WHERE c511_f(id) AND id < 2;
-- Under row security, a cheap leakproof condition runs before a costlier
-- policy; any other condition runs after the policies.
CREATE ROLE c511_reader;
GRANT ALL ON c511 TO c511_reader;
CREATE FUNCTION c511_p(v int) RETURNS bool LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE 'p %', v; RETURN v % 2 = 0; END $$;
CREATE FUNCTION c511_r(v int) RETURNS bool LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE 'r %', v; RETURN v > 2; END $$;
ALTER TABLE c511 ENABLE ROW LEVEL SECURITY;
CREATE POLICY c511_even ON c511 USING (c511_p(id));
SET ROLE c511_reader;
SELECT id FROM c511 WHERE c511_f(id) AND id < 4;
SELECT id FROM c511 WHERE id + 0 < 4;
UPDATE c511 SET a = a WHERE c511_f(id) AND id < 4;
DELETE FROM c511 WHERE id < 3 AND c511_f(id);
RESET ROLE;
CREATE POLICY c511_restrict ON c511 AS RESTRICTIVE USING (c511_r(id));
CREATE POLICY c511_cheap ON c511 USING (a > 0);
SET ROLE c511_reader;
SELECT id FROM c511 WHERE c511_f(id) AND id < 5;
SELECT id FROM c511 WHERE id + 0 < 5 AND c511_f(id);
RESET ROLE;
DROP TABLE c511;
DROP ROLE c511_reader;

-- EXPLAIN prints a relation's row-security conditions in its scan's Filter,
-- in PostgreSQL's order_qual_clauses order: each security qual (each
-- restrictive policy, then the OR of the permissive ones) has its own level
-- below the statement's conditions, and a leakproof condition cheaper than
-- ten operators takes level zero. Such a condition runs before the first
-- security qual when it costs less, and right after it otherwise.
CREATE ROLE c541_reader;
CREATE TABLE c541 (a int, b text, c int);
INSERT INTO c541 VALUES (1, 'x', 1), (2, 'y', 2), (3, 'z', 3);
GRANT SELECT ON c541 TO c541_reader;
ALTER TABLE c541 ENABLE ROW LEVEL SECURITY;
CREATE POLICY c541_permissive ON c541 USING (a > 0);
CREATE POLICY c541_restrictive ON c541 AS RESTRICTIVE USING (b <> 'q');
CREATE FUNCTION c541_noisy(int) RETURNS bool LANGUAGE plpgsql COST 1 AS $$
BEGIN RAISE NOTICE 'noisy %', $1; RETURN true; END $$;
SET ROLE c541_reader;
EXPLAIN (COSTS OFF) SELECT * FROM c541 WHERE c = 2;
EXPLAIN (COSTS OFF) SELECT * FROM c541 WHERE c < 3 AND c541_noisy(a);
EXPLAIN (COSTS OFF) SELECT * FROM c541;
EXPLAIN (COSTS OFF) SELECT * FROM c541 WHERE a + c + a + c + a + c > 0;
RESET ROLE;
CREATE TABLE c541_s (a int, b int);
INSERT INTO c541_s VALUES (1, 1), (2, 2);
GRANT SELECT, UPDATE, DELETE ON c541_s TO c541_reader;
ALTER TABLE c541_s ENABLE ROW LEVEL SECURITY;
CREATE FUNCTION c541_rp(int) RETURNS bool LANGUAGE plpgsql COST 1 AS $$
BEGIN RAISE NOTICE 'restrictive %', $1; RETURN true; END $$;
CREATE FUNCTION c541_pp(int) RETURNS bool LANGUAGE plpgsql COST 1 AS $$
BEGIN RAISE NOTICE 'permissive %', $1; RETURN true; END $$;
CREATE FUNCTION c541_lp(int) RETURNS bool LANGUAGE plpgsql LEAKPROOF COST 1 AS $$
BEGIN RAISE NOTICE 'leakproof %', $1; RETURN true; END $$;
CREATE FUNCTION c541_lq(int) RETURNS bool LANGUAGE plpgsql LEAKPROOF COST 0.5 AS $$
BEGIN RAISE NOTICE 'cheap leakproof %', $1; RETURN true; END $$;
CREATE POLICY c541_s_restrictive ON c541_s AS RESTRICTIVE USING (c541_rp(a));
CREATE POLICY c541_s_permissive ON c541_s USING (c541_pp(a));
SET ROLE c541_reader;
SELECT * FROM c541_s WHERE c541_lp(b) ORDER BY a;
EXPLAIN (COSTS OFF) SELECT * FROM c541_s WHERE c541_lp(b);
SELECT * FROM c541_s WHERE c541_lp(b) AND c541_lq(b) ORDER BY a;
EXPLAIN (COSTS OFF) SELECT * FROM c541_s WHERE c541_lp(b) AND c541_lq(b);
UPDATE c541_s SET a = a WHERE c541_lp(b) AND c541_lq(b);
DELETE FROM c541_s WHERE c541_lp(b) AND b > 1;
RESET ROLE;
DROP OWNED BY c541_reader;
DROP ROLE c541_reader;

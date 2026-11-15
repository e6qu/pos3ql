-- EXPLAIN of UPDATE and DELETE prints the target scan's row-security
-- conditions in its Filter, in PostgreSQL's order_qual_clauses order: the
-- write's own policies (each restrictive one, then the OR of the permissive
-- ones), then, when the statement reads the target's columns, the SELECT
-- policies not already listed, each at its own security level below the
-- statement's conditions; a cheap leakproof condition takes level zero.
CREATE ROLE c571_writer;
CREATE TABLE c571 (a int, b int, c text);
INSERT INTO c571 VALUES (1, 1, 'x'), (2, 2, 'y'), (3, 3, 'z');
GRANT SELECT, UPDATE, DELETE ON c571 TO c571_writer;
ALTER TABLE c571 ENABLE ROW LEVEL SECURITY;
CREATE POLICY c571_select ON c571 FOR SELECT USING (a > 0);
CREATE POLICY c571_update ON c571 FOR UPDATE USING (b < 10);
CREATE POLICY c571_update_r ON c571 AS RESTRICTIVE FOR UPDATE USING (c <> 'q');
CREATE POLICY c571_delete ON c571 FOR DELETE USING (b <> 7);
CREATE POLICY c571_all ON c571 USING (a <> 5);
CREATE FUNCTION c571_noisy(int) RETURNS bool LANGUAGE plpgsql COST 1 AS $$
BEGIN RAISE NOTICE 'noisy %', $1; RETURN true; END $$;
CREATE FUNCTION c571_leak(int) RETURNS bool LANGUAGE plpgsql LEAKPROOF COST 0.5 AS $$
BEGIN RAISE NOTICE 'leakproof %', $1; RETURN true; END $$;
SET ROLE c571_writer;
EXPLAIN (COSTS OFF) UPDATE c571 SET c = 'w';
EXPLAIN (COSTS OFF) UPDATE c571 SET c = 'w' WHERE a = 2;
EXPLAIN (COSTS OFF) UPDATE c571 SET c = c || 'w';
EXPLAIN (COSTS OFF) UPDATE c571 SET c = 'w' RETURNING a;
EXPLAIN (COSTS OFF) UPDATE c571 SET c = 'w' WHERE c571_noisy(a) AND b < 3;
EXPLAIN (COSTS OFF) UPDATE c571 SET c = 'w' WHERE c571_leak(a);
EXPLAIN (COSTS OFF) DELETE FROM c571;
EXPLAIN (COSTS OFF) DELETE FROM c571 WHERE b = 2;
EXPLAIN (COSTS OFF) DELETE FROM c571 WHERE c571_leak(b) AND c571_noisy(b);
EXPLAIN (COSTS OFF) DELETE FROM c571 RETURNING *;
-- VERBOSE prints the scan's new values and ctid, and the RETURNING list.
EXPLAIN (VERBOSE, COSTS OFF) UPDATE c571 SET c = 'w' WHERE a = 2;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE c571 x SET c = x.c || 'w', b = b + 1 RETURNING a, c;
EXPLAIN (VERBOSE, COSTS OFF) DELETE FROM c571 WHERE a = 2 RETURNING *;
UPDATE c571 SET c = 'w' WHERE c571_leak(a) AND c571_noisy(b) RETURNING a, c;
DELETE FROM c571 WHERE c571_leak(b) AND c571_noisy(b) RETURNING a;
RESET ROLE;
DROP OWNED BY c571_writer;
DROP ROLE c571_writer;

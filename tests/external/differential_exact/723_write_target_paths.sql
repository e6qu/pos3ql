-- An UPDATE or DELETE scans its target by the path PostgreSQL's costs
-- choose for the target alone, never an index-only scan, and changes the
-- rows in that path's order, which RETURNING shows. The rows stay under
-- the forced-spill run's 256 KiB memtable.
CREATE TABLE w723 (id int PRIMARY KEY, a int, b int, t text);
INSERT INTO w723 SELECT g, g % 30, (g * 13) % 200, 'x' FROM generate_series(1, 3000) g;
CREATE INDEX w723_a ON w723 (a);
CREATE INDEX w723_ab ON w723 (b DESC, a);
ANALYZE w723;
\set e 'EXPLAIN (COSTS OFF)'
:e UPDATE w723 SET t = 'y' WHERE id = 7;
:e UPDATE w723 SET t = 'y' WHERE a = 3;
:e UPDATE w723 SET t = 'y' WHERE b = 3 AND a = 9;
:e UPDATE w723 SET t = 'y' WHERE id < 20;
:e UPDATE w723 SET t = 'y' WHERE a = 3 OR id = 5;
:e UPDATE w723 SET t = 'y' WHERE t = 'z';
:e DELETE FROM w723 WHERE id IN (3, 9, 1);
:e DELETE FROM w723 WHERE b > 197 AND a < 2;
:e DELETE FROM w723 WHERE a = 3 AND b = 39;
UPDATE w723 SET t = 'y' WHERE id IN (9, 3, 1) RETURNING id;
UPDATE w723 SET t = 'z' WHERE b > 197 AND a < 4 RETURNING id, a, b;
DELETE FROM w723 WHERE id < 6 RETURNING id;
DELETE FROM w723 WHERE b = 199 AND a > 20 RETURNING id, a, b;

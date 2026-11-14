-- A compared subquery EXPLAIN shows as a subplan prints its comparison in
-- the condition: hashed when uncorrelated `IN` or `= ANY`, scanned from a
-- materialized copy for other uncorrelated operators.
CREATE TABLE sp_t (id int, v int);
CREATE TABLE sp_u (id int, w int);
INSERT INTO sp_t SELECT g, g % 7 FROM generate_series(1, 50) g;
INSERT INTO sp_u SELECT g, g % 5 FROM generate_series(1, 20) g;
ANALYZE sp_t;
ANALYZE sp_u;
EXPLAIN (COSTS OFF) SELECT * FROM sp_t WHERE id NOT IN (SELECT id FROM sp_u);
EXPLAIN (COSTS OFF) SELECT * FROM sp_t WHERE id = ANY (SELECT id FROM sp_u WHERE w > 1) OR v = 3;
EXPLAIN (COSTS OFF) SELECT * FROM sp_t WHERE NOT (id = ANY (SELECT id FROM sp_u));
EXPLAIN (COSTS OFF) SELECT * FROM sp_t WHERE id > ALL (SELECT id FROM sp_u);
EXPLAIN (COSTS OFF) SELECT * FROM sp_t t WHERE t.id IN (SELECT u.id FROM sp_u u WHERE u.w = t.v) OR t.v = 0;
EXPLAIN (COSTS OFF) SELECT * FROM sp_t t WHERE t.id NOT IN (SELECT u.id FROM sp_u u WHERE u.w = t.v);
EXPLAIN (COSTS OFF) SELECT id NOT IN (SELECT id FROM sp_u) FROM sp_t;
SET enable_material = off;
EXPLAIN (COSTS OFF) SELECT * FROM sp_t WHERE id > ALL (SELECT id FROM sp_u);
RESET enable_material;
SELECT count(*) FROM sp_t WHERE id NOT IN (SELECT id FROM sp_u);
SELECT count(*) FROM sp_t WHERE id > ALL (SELECT id FROM sp_u);
DROP TABLE sp_t, sp_u;

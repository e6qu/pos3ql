-- Merging a FROM subquery whose own FROM item has the name of another item
-- of the query: a NOT MATERIALIZED CTE read twice, a self-join through
-- derived tables, a table joined with a subquery over itself. The merged
-- item takes a name no SQL text can spell, so references keep their
-- bindings: correlated subqueries, whole-row output, USING, outer joins,
-- locking clauses and references to names that do not exist behave as in
-- PostgreSQL.
CREATE TABLE t (id int, v text);
INSERT INTO t SELECT g, 'v' || g FROM generate_series(1, 5) g;
CREATE TABLE t_1 (id int);
INSERT INTO t_1 VALUES (2), (4);
WITH c AS NOT MATERIALIZED (SELECT id, v FROM t WHERE id > 1)
SELECT a.id, b.v FROM c a JOIN c b ON a.id = b.id + 1 ORDER BY 1;
WITH c AS NOT MATERIALIZED (SELECT id, v FROM t)
SELECT a.id, b.v FROM c a LEFT JOIN c b ON a.id = b.id + 3 ORDER BY 1;
WITH c AS NOT MATERIALIZED (SELECT id, v FROM t)
SELECT count(*) FROM c a, c b, c d WHERE a.id = b.id AND b.id = d.id;
SELECT a.id, b.id FROM (SELECT id FROM t) a, (SELECT id FROM t) b
WHERE a.id = b.id ORDER BY 1;
SELECT t.id, s.v FROM t JOIN (SELECT id, v FROM t WHERE id < 4) s ON s.id = t.id ORDER BY 1;
SELECT t.id, s.v FROM t
JOIN (SELECT t.id, t.v FROM t
      WHERE t.id < 4 AND EXISTS (SELECT 1 FROM t t2 WHERE t2.id = t.id)) s
  ON s.id = t.id ORDER BY 1;
SELECT t.id, s.x FROM t
JOIN (SELECT id, (SELECT max(id) FROM t u WHERE u.id <= t.id) x FROM t) s
  ON s.id = t.id ORDER BY 1;
SELECT t.id, s.x FROM t
JOIN (SELECT id, (SELECT max(t_1.id) FROM t_1 WHERE t_1.id <= t.id) x FROM t) s
  ON s.id = t.id ORDER BY 1;
SELECT * FROM (SELECT * FROM t) a JOIN (SELECT * FROM t) b USING (id) ORDER BY 1;
SELECT a.*, b.* FROM (SELECT * FROM t) a JOIN (SELECT * FROM t) b ON a.id = b.id
ORDER BY 1 LIMIT 2;
SELECT a, b FROM (SELECT * FROM t) a JOIN (SELECT * FROM t) b ON a.id = b.id ORDER BY 1;
SELECT a.id FROM (SELECT * FROM t) a JOIN (SELECT * FROM t_1) b ON a.id = b.id
JOIN (SELECT * FROM t_1) c ON c.id = b.id ORDER BY 1;
SELECT t_1.id FROM (SELECT * FROM t) a, (SELECT * FROM t) b;
SELECT t_1.id FROM (SELECT * FROM t) a, (SELECT * FROM t) b, t_1 WHERE a.id = b.id AND b.id = t_1.id
ORDER BY 1;
SELECT a.id FROM (SELECT * FROM t) a LEFT JOIN (SELECT * FROM t) b ON a.id = b.id + 10
ORDER BY 1;
SELECT a.id FROM (SELECT * FROM t) a, (SELECT * FROM t) b WHERE a.id = b.id AND a.id < 3
ORDER BY 1 FOR UPDATE OF b;
SELECT t.v FROM (SELECT * FROM t) a, (SELECT * FROM t) b WHERE a.id = b.id;
SELECT b.v FROM t, (SELECT * FROM t WHERE id = 3) b WHERE t.id = b.id;

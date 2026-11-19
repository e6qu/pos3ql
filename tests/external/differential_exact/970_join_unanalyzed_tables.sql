-- Joins of tables never analyzed plan by PostgreSQL's default size and
-- selectivity estimates.
CREATE TABLE u1(id integer); INSERT INTO u1 VALUES (1);
CREATE TABLE u2(id integer, k int, t text); INSERT INTO u2 SELECT g, g % 7, 'x' || g FROM generate_series(1,300) g;
CREATE TABLE u3(id integer, k int); INSERT INTO u3 SELECT g, g % 5 FROM generate_series(1,50) g;
CREATE INDEX u3_id ON u3(id);
EXPLAIN SELECT a.id + c.id FROM u1 a, u1 b, u1 c;
EXPLAIN SELECT * FROM u2 JOIN u3 ON u2.id = u3.id;
EXPLAIN SELECT * FROM u2 JOIN u3 ON u2.k = u3.k WHERE u2.id < 10;
EXPLAIN SELECT * FROM u2 LEFT JOIN u3 ON u2.id = u3.id JOIN u1 ON u1.id = u3.k;
EXPLAIN SELECT count(*) FROM u2 a JOIN u2 b ON a.t = b.t;
EXPLAIN SELECT * FROM u2 WHERE EXISTS (SELECT 1 FROM u3 WHERE u3.k = u2.k);
SELECT count(*) FROM u2 JOIN u3 ON u2.k = u3.k WHERE u2.id < 10;
ANALYZE u3;
EXPLAIN SELECT * FROM u2 JOIN u3 ON u2.id = u3.id;
EXPLAIN (COSTS OFF) SELECT a.id + c.id FROM u1 a, u1 b, u1 c, u1 d, u1 e, u1 f, u1 g, u1 h, u1 i, u1 j, u1 k, u1 l, u1 m;

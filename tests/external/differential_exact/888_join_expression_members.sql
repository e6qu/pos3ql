-- Join conditions whose sides are expressions of one relation's columns
-- (a.k + 1 = b.k) are equivalence class members, as PostgreSQL 18's
-- process_equivalence makes them: they hash, merge and key a Memoize
-- cache, estimated without statistics (get_variable_numdistinct), each
-- computed by its relation's scan; an ORDER BY of such an expression
-- takes its class's order, and window and ordered-aggregate keys that are
-- expressions sort by them. A full join's merged column (or a COALESCE of
-- its two compared columns) equated with a constant equates each side with
-- it instead (reconsider_full_join_clause).
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/equivclass.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/selfuncs.c
CREATE TABLE x888 (k int, v int, t text);
INSERT INTO x888 SELECT g, g % 17, 'n' || (g % 23) FROM generate_series(1, 1200) g;
CREATE TABLE y888 (k int, w int, t text);
INSERT INTO y888 SELECT g, g % 11, 'N' || (g % 23) FROM generate_series(1, 400) g;
ALTER TABLE y888 ADD PRIMARY KEY (k);
CREATE TABLE z888 (k int, u int);
INSERT INTO z888 SELECT g % 300, g FROM generate_series(1, 900) g;
CREATE INDEX z888_k ON z888 (k);
CREATE TABLE q888 (a int, b int);
INSERT INTO q888 SELECT g, g % 40 FROM generate_series(1, 600) g;
CREATE INDEX q888_m ON q888 ((a % 50));
CREATE TABLE fa888 (k int, a int);
INSERT INTO fa888 SELECT g, g % 7 FROM generate_series(1, 500) g;
CREATE TABLE fb888 (k int, b int);
INSERT INTO fb888 SELECT g * 2, g % 5 FROM generate_series(1, 400) g;
ANALYZE x888;
ANALYZE y888;
ANALYZE z888;
ANALYZE q888;
ANALYZE fa888;
ANALYZE fb888;
EXPLAIN SELECT x888.v, y888.w FROM x888 JOIN y888 ON x888.k + 1 = y888.k;
EXPLAIN SELECT x888.v, y888.w FROM x888 JOIN y888 ON x888.k + 1 = y888.k ORDER BY x888.k + 1;
EXPLAIN SELECT x888.v FROM x888 JOIN y888 ON x888.k + 1 = y888.k ORDER BY x888.k + 1 LIMIT 5;
EXPLAIN SELECT y888.k, x888.v FROM x888 JOIN y888 ON x888.k + 1 = y888.k ORDER BY x888.k + 1;
EXPLAIN SELECT x888.v, y888.w FROM x888 JOIN y888 ON lower(x888.t) = lower(y888.t) WHERE y888.k < 30;
EXPLAIN SELECT y888.w, z888.u FROM y888 JOIN z888 ON z888.k = y888.w * 2 WHERE y888.k < 20;
EXPLAIN SELECT x888.v, y888.w FROM x888 LEFT JOIN y888 ON x888.k * 2 = y888.k WHERE x888.v = 3;
EXPLAIN SELECT x888.v, y888.w FROM x888 JOIN y888 ON x888.k + 1 = y888.k WHERE y888.k = 7;
EXPLAIN SELECT x888.v FROM x888 JOIN y888 ON x888.k * 2 = y888.k JOIN z888 ON z888.k = y888.w * 3;
SET enable_hashjoin = off;
EXPLAIN SELECT x888.v, y888.w FROM x888 JOIN y888 ON x888.k + 1 = y888.k;
SELECT x888.k, y888.w FROM x888 JOIN y888 ON x888.k + 1 = y888.k ORDER BY x888.k LIMIT 6;
RESET enable_hashjoin;
EXPLAIN SELECT k, row_number() OVER (PARTITION BY v % 5 ORDER BY k + 1) FROM x888;
EXPLAIN SELECT k, sum(v) OVER (ORDER BY lower(t)) FROM x888;
EXPLAIN SELECT x888.k, rank() OVER (PARTITION BY y888.w + 1 ORDER BY x888.k) FROM x888 JOIN y888 ON x888.k = y888.k;
EXPLAIN SELECT v, string_agg(t, ',' ORDER BY lower(t)) FROM x888 GROUP BY v;
EXPLAIN SELECT v, count(DISTINCT k % 7) FROM x888 GROUP BY v;
EXPLAIN SELECT y888.w, array_agg(x888.k ORDER BY x888.k * 2) FROM x888 JOIN y888 ON x888.k = y888.k GROUP BY y888.w;
SELECT x888.k, y888.w FROM x888 JOIN y888 ON x888.k + 1 = y888.k ORDER BY x888.k + 1 LIMIT 5;
SELECT count(*), sum(x888.v) FROM x888 JOIN y888 ON lower(x888.t) = lower(y888.t) WHERE y888.k < 30;
SELECT y888.w, z888.u FROM y888 JOIN z888 ON z888.k = y888.w * 2 WHERE y888.k < 20 ORDER BY y888.w, z888.u LIMIT 8;
SELECT count(*), count(y888.w) FROM x888 LEFT JOIN y888 ON x888.k * 2 = y888.k WHERE x888.v = 3;
SELECT x888.k, row_number() OVER (PARTITION BY x888.v % 5 ORDER BY x888.k + 1) FROM x888 WHERE x888.k < 30 ORDER BY x888.k LIMIT 10;
SELECT v, string_agg(t, ',' ORDER BY lower(t)) FROM x888 WHERE k < 60 GROUP BY v ORDER BY v LIMIT 4;
SELECT y888.w, array_agg(x888.k ORDER BY x888.k * 2) FROM x888 JOIN y888 ON x888.k = y888.k WHERE y888.k < 50 GROUP BY y888.w ORDER BY y888.w LIMIT 3;
EXPLAIN SELECT y888.k, q888.b FROM y888 JOIN q888 ON q888.a % 50 = y888.w;
EXPLAIN SELECT y888.k, q888.b FROM y888 JOIN q888 ON q888.a % 50 = y888.k WHERE y888.w = 3;
SELECT y888.k, q888.b FROM y888 JOIN q888 ON q888.a % 50 = y888.w WHERE y888.k < 4 ORDER BY y888.k, q888.b LIMIT 8;
EXPLAIN SELECT * FROM fa888 FULL JOIN fb888 USING (k) WHERE k = 30;
EXPLAIN SELECT * FROM fa888 FULL JOIN fb888 ON fa888.k = fb888.k WHERE COALESCE(fa888.k, fb888.k) = 30;
SELECT * FROM fa888 FULL JOIN fb888 USING (k) WHERE k = 30;
SELECT * FROM fa888 FULL JOIN fb888 USING (k) WHERE k = 31;
SELECT * FROM fa888 FULL JOIN fb888 USING (k) WHERE k = 800;

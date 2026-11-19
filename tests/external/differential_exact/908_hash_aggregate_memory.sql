-- PostgreSQL 18 EXPLAIN ANALYZE of a HashAggregate: its table's memory
-- (hash_agg_update_metrics: the meta context with the bucket array, the
-- entries and the by-reference transition states), over one table and over a
-- join, for GROUP BY and DISTINCT. The groups it estimates size the table
-- (estimate_num_groups, including a DISTINCT's), and an aggregate whose
-- groups outgrow hash_mem spills (hash_agg_check_limits), which fails with
-- 0A000 along with transition states and group estimates outside the model.
CREATE TABLE t908 (a int, b int, c text);
INSERT INTO t908 SELECT g % 700, g % 50, repeat('x', g % 20) FROM generate_series(1, 3000) g;
CREATE TABLE u908 (a int, e int);
INSERT INTO u908 SELECT g % 700, g FROM generate_series(1, 2000) g;
ANALYZE t908;
ANALYZE u908;
SET enable_sort = off;
CREATE FUNCTION hashed908(query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE
  line text;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
    CONTINUE WHEN line !~ '(Aggregate|Group Key|Batches)';
    RETURN NEXT line;
  END LOOP;
END
$$;

SET work_mem = '4MB';
SELECT hashed908($q$SELECT DISTINCT a FROM t908$q$);
SELECT hashed908($q$SELECT DISTINCT b FROM t908$q$);
SELECT hashed908($q$SELECT DISTINCT a, b FROM t908$q$);
SELECT hashed908($q$SELECT DISTINCT c FROM t908$q$);
SELECT hashed908($q$SELECT a, count(*) FROM t908 GROUP BY a$q$);
SELECT hashed908($q$SELECT b, count(*) FROM t908 GROUP BY b$q$);
SELECT hashed908($q$SELECT a, b, count(*) FROM t908 GROUP BY a, b$q$);
SELECT hashed908($q$SELECT a, sum(b) FROM t908 GROUP BY a$q$);
SELECT hashed908($q$SELECT a, avg(b) FROM t908 GROUP BY a$q$);
SELECT hashed908($q$SELECT a, min(b), max(b) FROM t908 GROUP BY a$q$);
SELECT hashed908($q$SELECT b, count(*) FROM t908 GROUP BY b HAVING count(*) > 3$q$);
SELECT hashed908($q$SELECT a, c, count(*) FROM t908 GROUP BY a, c$q$);
SELECT hashed908($q$SELECT t908.a, count(*) FROM t908 JOIN u908 ON t908.a = u908.a GROUP BY t908.a$q$);
SELECT hashed908($q$SELECT DISTINCT t908.a, u908.e FROM t908 JOIN u908 ON t908.a = u908.a$q$);
SELECT hashed908($q$SELECT t908.a, u908.e, count(*) FROM t908 JOIN u908 ON t908.a = u908.a GROUP BY t908.a, u908.e$q$);
SELECT hashed908($q$SELECT t908.b, sum(u908.e) FROM t908 JOIN u908 ON t908.a = u908.a GROUP BY t908.b$q$);
SET work_mem = '1MB';
SELECT hashed908($q$SELECT DISTINCT a FROM t908$q$);
SELECT hashed908($q$SELECT DISTINCT b FROM t908$q$);
SELECT hashed908($q$SELECT DISTINCT a, b FROM t908$q$);
SELECT hashed908($q$SELECT DISTINCT c FROM t908$q$);
SELECT hashed908($q$SELECT a, count(*) FROM t908 GROUP BY a$q$);
SELECT hashed908($q$SELECT b, count(*) FROM t908 GROUP BY b$q$);
SELECT hashed908($q$SELECT a, b, count(*) FROM t908 GROUP BY a, b$q$);
SELECT hashed908($q$SELECT a, sum(b) FROM t908 GROUP BY a$q$);
SELECT hashed908($q$SELECT a, avg(b) FROM t908 GROUP BY a$q$);
SELECT hashed908($q$SELECT a, min(b), max(b) FROM t908 GROUP BY a$q$);
SELECT hashed908($q$SELECT b, count(*) FROM t908 GROUP BY b HAVING count(*) > 3$q$);
SELECT hashed908($q$SELECT a, c, count(*) FROM t908 GROUP BY a, c$q$);
SELECT hashed908($q$SELECT t908.a, count(*) FROM t908 JOIN u908 ON t908.a = u908.a GROUP BY t908.a$q$);
SELECT hashed908($q$SELECT DISTINCT t908.a, u908.e FROM t908 JOIN u908 ON t908.a = u908.a$q$);
SELECT hashed908($q$SELECT t908.a, u908.e, count(*) FROM t908 JOIN u908 ON t908.a = u908.a GROUP BY t908.a, u908.e$q$);
SELECT hashed908($q$SELECT t908.b, sum(u908.e) FROM t908 JOIN u908 ON t908.a = u908.a GROUP BY t908.b$q$);

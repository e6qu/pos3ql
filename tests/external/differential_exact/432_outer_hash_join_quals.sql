-- PostgreSQL places an outer join's conditions where they can run first: a
-- WHERE condition reading only the side the join never null-extends filters
-- that side's scan, and an ON condition reading only the null-extended side
-- filters its scan, before the join hashes or probes their rows. The rest
-- of the ON condition is the join's Hash Cond and Join Filter; the rest of
-- the WHERE condition filters the join's output.
CREATE TABLE x432 (a int, b int);
CREATE TABLE y432 (a int, b int);
INSERT INTO x432 SELECT g, g FROM generate_series(1, 10) g;
INSERT INTO y432 SELECT g, g FROM generate_series(5, 200) g;
ANALYZE x432;
ANALYZE y432;
CREATE FUNCTION analyzed432(query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE
  line text;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
    CONTINUE WHEN line ~ '^\s*(Buckets|Sort Method|Batches|Storage):';
    RETURN NEXT line;
  END LOOP;
END
$$;
SELECT analyzed432('SELECT * FROM x432 RIGHT JOIN y432 ON x432.a = y432.a WHERE y432.b < 12');
SELECT * FROM x432 RIGHT JOIN y432 ON x432.a = y432.a WHERE y432.b < 12 ORDER BY y432.a;
SELECT analyzed432('SELECT * FROM x432 RIGHT JOIN y432 ON x432.a = y432.a AND x432.b > 6 WHERE y432.b < 12');
SELECT * FROM x432 RIGHT JOIN y432 ON x432.a = y432.a AND x432.b > 6 WHERE y432.b < 12 ORDER BY y432.a;
SELECT analyzed432('SELECT * FROM x432 RIGHT JOIN y432 ON x432.a = y432.a AND y432.b > 6 WHERE y432.b < 12');
SELECT * FROM x432 RIGHT JOIN y432 ON x432.a = y432.a AND y432.b > 6 WHERE y432.b < 12 ORDER BY y432.a;
SELECT analyzed432('SELECT * FROM x432 RIGHT JOIN y432 ON x432.a = y432.a WHERE y432.b < 12 AND x432.b IS NULL');
SELECT * FROM x432 RIGHT JOIN y432 ON x432.a = y432.a WHERE y432.b < 12 AND x432.b IS NULL ORDER BY y432.a;
SELECT analyzed432('SELECT * FROM y432 LEFT JOIN x432 ON x432.a = y432.a AND x432.b > 6 WHERE y432.b < 12');
SELECT * FROM y432 LEFT JOIN x432 ON x432.a = y432.a AND x432.b > 6 WHERE y432.b < 12 ORDER BY y432.a;
SELECT analyzed432('SELECT * FROM x432 FULL JOIN y432 ON x432.a = y432.a WHERE y432.b < 12');
SELECT * FROM x432 FULL JOIN y432 ON x432.a = y432.a WHERE y432.b < 12 ORDER BY y432.a;
SELECT analyzed432('SELECT * FROM y432 FULL JOIN x432 ON x432.a = y432.a AND y432.b > 6');
SELECT count(*), count(x432.a), count(y432.a) FROM y432 FULL JOIN x432 ON x432.a = y432.a AND y432.b > 6;

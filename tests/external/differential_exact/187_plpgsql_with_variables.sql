-- WITH queries of PL/pgSQL statements read the routine's variables, also when
-- materialized: RETURN QUERY, SELECT INTO and FOR loops.
CREATE TABLE wv184 (a int, b text);
CREATE FUNCTION wv184_rows(n int) RETURNS SETOF int LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY WITH c AS MATERIALIZED (SELECT n AS v UNION ALL SELECT n + 1) SELECT v FROM c ORDER BY v;
END $$;
SELECT * FROM wv184_rows(5);
DO $$ DECLARE k int := 7; t text := 'x'; r record; s int; BEGIN
  WITH c AS MATERIALIZED (SELECT k * 2 AS v) SELECT v INTO s FROM c;
  RAISE NOTICE 's=%', s;
  WITH c AS (SELECT k + 1 AS v, t || 'y' AS w) SELECT c.v + d.v INTO s FROM c, c AS d;
  RAISE NOTICE 's=%', s;
  FOR r IN WITH c AS MATERIALIZED (SELECT k - 1 AS v) SELECT v FROM c LOOP
    RAISE NOTICE 'r=%', r.v;
  END LOOP;
END $$;
SELECT * FROM wv184 ORDER BY a;
DROP FUNCTION wv184_rows(int);
DROP TABLE wv184;

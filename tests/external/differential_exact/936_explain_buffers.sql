-- EXPLAIN's BUFFERS counts each node's page accesses: on by default under
-- ANALYZE (PostgreSQL 18), every node showing its own and its inputs' pages,
-- a counter shown only above zero, and local pages for a temporary table. The counts are each server's own (an
-- 8 kB page of pos3ql's modeled heap and index layout), so the fixture
-- compares where they appear and which counters show, with the numbers
-- masked. A first run brings PostgreSQL's pages into its shared buffers;
-- PostgreSQL's planning reads catalog pages that pos3ql keeps resident, so
-- its `Planning:` buffers are left out.
CREATE TABLE bt936 (a int, b text);
INSERT INTO bt936 SELECT g, 'x' || g FROM generate_series(1, 4000) g;
CREATE INDEX bt936_a ON bt936 (a);
CREATE TABLE bu936 (k int, v int);
INSERT INTO bu936 SELECT g % 300, g FROM generate_series(1, 900) g;
CREATE INDEX bu936_k ON bu936 (k);
VACUUM ANALYZE bt936, bu936;
CREATE FUNCTION buffers936(options text, query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE
  line text;
  planning boolean := false;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (' || options || ') ' || query LOOP
  END LOOP;
  FOR line IN EXECUTE 'EXPLAIN (' || options || ') ' || query LOOP
    IF line = 'Planning:' THEN
      planning := true;
      CONTINUE;
    END IF;
    IF planning AND line ~ '^  Buffers:' THEN
      CONTINUE;
    END IF;
    line := regexp_replace(line, '(hit|read|dirtied|written)=\d+', '\1=N', 'g');
    line := regexp_replace(line, '(Blocks": |Blocks: )\d+', '\1N', 'g');
    line := regexp_replace(line, '(Memory: |Memory Usage: )\d+kB', '\1NkB', 'g');
    RETURN NEXT line;
  END LOOP;
END
$$;
SELECT buffers936('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'SELECT * FROM bt936 WHERE b = ''x5''');
SELECT buffers936('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'SELECT * FROM bt936 LIMIT 5');
SELECT buffers936('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'SELECT * FROM bt936 WHERE a < 300');
SELECT buffers936('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'SELECT a FROM bt936 WHERE a < 1000');
SELECT buffers936('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'SELECT * FROM bu936 WHERE k = 7');
SELECT buffers936('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'SELECT bu936.k, bt936.a FROM bt936 JOIN bu936 ON bt936.a = bu936.v + 1 ORDER BY bu936.k LIMIT 10');
SELECT buffers936('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'SELECT * FROM bu936 JOIN bt936 ON bt936.a = bu936.k WHERE bu936.v < 30');
SELECT buffers936('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'SELECT bu936.v, bt936.b FROM bu936 JOIN bt936 ON bt936.a = bu936.v WHERE bu936.k = 7');
SELECT buffers936('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'WITH w AS MATERIALIZED (SELECT k FROM bu936 WHERE v < 100) SELECT count(*) FROM w');
SELECT buffers936('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF', 'SELECT * FROM bt936 WHERE a = 9');
SELECT buffers936('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, SERIALIZE', 'SELECT * FROM bt936 WHERE a < 20');
SELECT buffers936('BUFFERS, COSTS OFF', 'SELECT * FROM bt936 WHERE a = 7');
SELECT buffers936('BUFFERS, COSTS OFF, FORMAT JSON', 'SELECT * FROM bt936 WHERE a = 7');
SELECT buffers936('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, FORMAT YAML', 'SELECT * FROM bt936 WHERE a = 7');
SELECT buffers936('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'SELECT 1 + 1');
CREATE TEMP TABLE tt936 AS SELECT g AS a FROM generate_series(1, 1000) g;
ANALYZE tt936;
SELECT buffers936('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'SELECT * FROM tt936 WHERE a > 990');
DROP TABLE tt936;
DROP FUNCTION buffers936(text, text);
DROP TABLE bt936, bu936;

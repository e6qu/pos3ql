-- PostgreSQL 18 EXPLAIN ANALYZE of a sort that outgrows work_mem: the sort
-- writes quicksorted runs to tapes (tuplesort.c inittapes, dumptuples) and
-- merges them, so it reports "external merge" with the temporary file's size
-- in kB (LogicalTapeSetBlocks), including a merge of more runs than tapes.
-- A bounded sort that fills work_mem past its bound keeps its heap.
CREATE TABLE t906 (a int, b text, c numeric);
INSERT INTO t906 SELECT (g * 7919) % 10007, CASE WHEN g % 11 = 0 THEN NULL ELSE repeat(chr(97 + g % 26), 1 + g % 60) END, (g * 13)::numeric / 7 FROM generate_series(1, 3000) g;
CREATE TABLE u906 (a int, b int);
INSERT INTO u906 SELECT g % 97, g FROM generate_series(1, 3000) g;
ANALYZE t906;
ANALYZE u906;
CREATE FUNCTION sorted906(query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE
  line text;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
    CONTINUE WHEN line !~ 'Sort (Method|Key)';
    RETURN NEXT line;
  END LOOP;
END
$$;
SET work_mem = '64kB';
SELECT sorted906('SELECT a FROM t906 ORDER BY a');
SELECT sorted906('SELECT a, b FROM t906 ORDER BY a');
SELECT sorted906('SELECT * FROM t906 ORDER BY c');
SELECT sorted906('SELECT * FROM t906 ORDER BY b, a');
SELECT sorted906('SELECT a, b FROM u906 ORDER BY a');
SELECT sorted906('SELECT a, b FROM u906 ORDER BY b DESC');
SELECT sorted906('SELECT a, b FROM u906 ORDER BY a LIMIT 10');
SELECT sorted906('SELECT a, b FROM u906 ORDER BY a LIMIT 1000');
SELECT sorted906('SELECT a, b FROM u906 ORDER BY b LIMIT 2500');
SET work_mem = '256kB';
SELECT sorted906('SELECT * FROM t906 ORDER BY b, a');
SELECT sorted906('SELECT a, b FROM u906 ORDER BY b DESC');
SELECT sorted906('SELECT a FROM t906 ORDER BY a');
SET work_mem = '1MB';
SELECT sorted906('SELECT * FROM t906 ORDER BY b, a');
SELECT sorted906('SELECT a, b FROM u906 ORDER BY b DESC');

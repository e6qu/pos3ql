-- EXPLAIN ANALYZE's BUFFERS of writes: the `ModifyTable` node counts the
-- pages its write reads and changes with its scan's, and a new page the
-- write adds counts as dirtied and written. The counts are each server's
-- own (pos3ql's modeled heap and btree layout), so the fixture compares
-- where they appear and which counters show, with the numbers masked. Each
-- write runs twice so PostgreSQL's pages are in its shared buffers and
-- dirty; its `Planning:` buffers read catalog pages pos3ql keeps resident.
CREATE TABLE wh939 (id int, v text);
INSERT INTO wh939 SELECT g, 'x' || g FROM generate_series(1, 1000) g;
CREATE TABLE wk939 (id int PRIMARY KEY, v text, w int);
CREATE INDEX wk939_w ON wk939 (w);
INSERT INTO wk939 SELECT g, 'y', g FROM generate_series(1, 2000) g;
CREATE TABLE ws939 (id serial, v text);
CREATE TABLE wm939 (id int, v text);
INSERT INTO wm939 SELECT g, 'm' FROM generate_series(1, 300) g;
INSERT INTO ws939 (v) VALUES ('a');
ANALYZE wh939, wk939, ws939, wm939;
SELECT count(*) FROM wh939;
SELECT count(*) FROM wk939;
CREATE FUNCTION buffers939(options text, query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
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
    RETURN NEXT line;
  END LOOP;
END
$$;
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'INSERT INTO wh939 VALUES (5000, ''a'')');
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'INSERT INTO wh939 SELECT g, ''z'' FROM generate_series(1, 1000) g');
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'INSERT INTO wk939 SELECT g + 3000, ''y'', g FROM generate_series(1, 2) g ON CONFLICT DO NOTHING');
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'INSERT INTO ws939 (v) VALUES (''b'')');
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'INSERT INTO ws939 (v) VALUES (''b''), (''c'')');
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'UPDATE wh939 SET v = ''u'' WHERE id = 5');
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'UPDATE wk939 SET v = ''u'' WHERE id = 7');
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'UPDATE wk939 SET w = -w WHERE id = 8');
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'UPDATE wk939 SET v = v WHERE id < 400');
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'DELETE FROM wh939 WHERE id = 9');
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'DELETE FROM wk939 WHERE id = 10');
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'DELETE FROM wk939 WHERE id = 10 RETURNING *');
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, FORMAT JSON', 'UPDATE wk939 SET v = ''j'' WHERE id = 11');
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'INSERT INTO wk939 VALUES (12, ''c'', 0) ON CONFLICT (id) DO UPDATE SET v = ''d''');
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'MERGE INTO wm939 USING (VALUES (13, ''m''), (5000, ''n'')) AS s (id, v) ON wm939.id = s.id WHEN MATCHED THEN UPDATE SET v = s.v WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.v)');
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, FORMAT YAML', 'INSERT INTO wk939 VALUES (12, ''e'', 0) ON CONFLICT ON CONSTRAINT wk939_pkey DO NOTHING');
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'UPDATE wm939 SET v = s.v FROM wh939 s WHERE wm939.id = s.id AND s.id < 3');
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF', 'DELETE FROM wh939 WHERE id = 14');
CREATE TEMP TABLE wt939 (id int, v text);
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'INSERT INTO wt939 SELECT g, ''t'' FROM generate_series(1, 500) g');
SELECT buffers939('ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF', 'DELETE FROM wt939 WHERE id = 3');
DROP TABLE wt939;
DROP FUNCTION buffers939(text, text);
DROP TABLE wh939, wk939, ws939, wm939;

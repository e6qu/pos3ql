-- UPDATE ... FROM and DELETE ... USING with several FROM items hash the
-- first one on the target's equality and join the later items to it, with
-- each target row changed once.
CREATE TABLE t373 (id int, v int, note text);
CREATE TABLE s373 (id int, w int);
CREATE TABLE u373 (k int, z text);
INSERT INTO t373 SELECT g, g, 'n' FROM generate_series(1, 60) g;
INSERT INTO s373 SELECT g, g + 100 FROM generate_series(1, 50) g;
INSERT INTO u373 SELECT g + 100, 'z' || g FROM generate_series(1, 30) g;
WITH x AS (
  UPDATE t373 SET note = u373.z FROM s373 JOIN u373 ON s373.w = u373.k
  WHERE t373.v = s373.id RETURNING t373.id, t373.note
) SELECT count(*), min(id), max(id) FROM x;
WITH d AS (
  DELETE FROM t373 USING s373 LEFT JOIN u373 ON s373.w = u373.k
  WHERE t373.v = s373.id AND t373.id BETWEEN 25 AND 35 RETURNING t373.id, u373.z
) SELECT * FROM d ORDER BY id;
SELECT count(*), count(*) FILTER (WHERE note <> 'n') FROM t373;

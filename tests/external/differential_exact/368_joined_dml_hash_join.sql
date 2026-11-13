-- UPDATE ... FROM and DELETE ... USING hash a FROM table joined by equality,
-- keep the last joined FROM row as PostgreSQL's hash chains do, and print the
-- hash join's condition beside its join filter. A join column fixed by a
-- constant leaves no hash key, and conflicting implicit collations fail.
CREATE TABLE t368 (id int PRIMARY KEY, v int, note text);
CREATE TABLE s368 (id bigint, w int, tag text);
INSERT INTO t368 SELECT g, g % 7, 'n' || g FROM generate_series(1, 1000) g;
INSERT INTO s368 SELECT g % 10, g, 't' || g FROM generate_series(1, 40) g;
INSERT INTO s368 VALUES (NULL, 1, 'null key');
ANALYZE t368;
ANALYZE s368;
EXPLAIN (COSTS OFF) UPDATE t368 SET note = s368.tag FROM s368 WHERE t368.v = s368.id;
EXPLAIN (COSTS OFF) UPDATE t368 SET note = s368.tag FROM s368
  WHERE t368.v = s368.id AND t368.id > s368.w AND s368.w > 20;
EXPLAIN (COSTS OFF) DELETE FROM t368 USING s368 WHERE t368.v = s368.id AND s368.tag LIKE 't3%';
EXPLAIN (COSTS OFF) UPDATE t368 SET note = 'x' FROM s368 WHERE t368.v = s368.id AND s368.id = 1;
WITH u AS (
  UPDATE t368 SET note = s368.tag FROM s368
  WHERE t368.v = s368.id AND s368.w > 20 AND t368.id < 9
  RETURNING t368.id, t368.note, s368.w
) SELECT * FROM u ORDER BY id;
WITH d AS (
  DELETE FROM t368 USING s368 WHERE t368.v = s368.id AND s368.tag LIKE 't3%'
  AND t368.id IN (3, 10, 17) RETURNING t368.id, s368.tag
) SELECT * FROM d ORDER BY id;
SELECT count(*), min(note), max(note) FROM t368;
CREATE TABLE c368 (k text COLLATE "C");
CREATE TABLE p368 (k text COLLATE "POSIX");
INSERT INTO c368 VALUES ('t1'), ('zz');
INSERT INTO p368 VALUES ('t1');
UPDATE c368 SET k = s368.tag || '!' FROM s368 WHERE c368.k = s368.tag;
SELECT * FROM c368 ORDER BY k;
UPDATE c368 SET k = 'x' FROM p368 WHERE c368.k = p368.k;
SELECT count(*) FROM c368 JOIN p368 ON c368.k = p368.k;
SELECT c368.k < p368.k FROM c368, p368;

-- Two scans of one table joined on a unique key merge into one scan, as
-- PostgreSQL 18's remove_useless_self_joins merges them.
CREATE TABLE sj (a int UNIQUE, b int, c int UNIQUE);
INSERT INTO sj VALUES (1, NULL, 2), (NULL, 2, NULL), (2, 1, 1), (3, 1, 3);
ANALYZE sj;
CREATE TABLE sjk (id int PRIMARY KEY, v int, w text);
INSERT INTO sjk SELECT g, g % 7, 'w' || g FROM generate_series(1, 200) g;
ANALYZE sjk;
CREATE TABLE sjo (id int, ref int);
INSERT INTO sjo SELECT g, g % 50 FROM generate_series(1, 100) g;
ANALYZE sjo;

EXPLAIN (COSTS OFF) SELECT p.* FROM sj p, sj q WHERE q.a = p.a AND q.b < 2;
SELECT p.* FROM sj p, sj q WHERE q.a = p.a AND q.b < 2;
EXPLAIN SELECT p.* FROM sj p, sj q WHERE q.a = p.a AND q.b < 2;
EXPLAIN (COSTS OFF) SELECT p.* FROM sj p, sj q WHERE q.a = p.a AND q.b = q.a - 1;
SELECT p.* FROM sj p, sj q WHERE q.a = p.a AND q.b = q.a - 1;
EXPLAIN SELECT p.* FROM sj p, sj q WHERE q.a = p.a AND q.b = q.a - 1;
SET enable_self_join_elimination = off;
EXPLAIN SELECT p.* FROM sj p, sj q WHERE q.a = p.a AND q.b = q.a - 1;
RESET enable_self_join_elimination;
-- A column compared with an expression of its own relation's columns.
EXPLAIN SELECT * FROM sjk WHERE v = id - 1;
EXPLAIN SELECT * FROM sjk WHERE v < id + 1 AND id > 150;
EXPLAIN SELECT * FROM sjk WHERE id = v * 2;
SELECT count(*) FROM sjk WHERE v = id % 5;
-- Different unique columns: no removal.
EXPLAIN (COSTS OFF) SELECT * FROM sj t1, sj t2 WHERE t1.a = t2.c AND t1.b IS NOT NULL;
-- Classes equating two columns of one scan.
EXPLAIN (COSTS OFF) SELECT * FROM sj p, sj q WHERE q.a = p.a AND q.b = q.a;
EXPLAIN (COSTS OFF) SELECT * FROM sj t1, sj t2 WHERE t1.a = t1.b AND t1.b = t2.b;
EXPLAIN (COSTS OFF) SELECT * FROM sj t1, sj t2 WHERE t1.b = t2.b AND t1.a = t1.b;
EXPLAIN (COSTS OFF) SELECT * FROM sj t1, sj t2 WHERE t1.a = t1.b AND t1.b = t2.b AND t2.b = t2.a;
-- Three scans.
EXPLAIN (COSTS OFF) SELECT * FROM sj t1 JOIN sj t2 ON t1.a = t2.a AND t1.b = t2.b JOIN sj t3 ON t2.a = t3.a;
SELECT * FROM sj t1 JOIN sj t2 ON t1.a = t2.a AND t1.b = t2.b JOIN sj t3 ON t2.a = t3.a ORDER BY 1;
-- Under an outer join.
CREATE TABLE sjz (q1 int, q2 int);
INSERT INTO sjz VALUES (1, 10), (2, 20), (5, 50);
ANALYZE sjz;
EXPLAIN (COSTS OFF) SELECT * FROM sj x JOIN sj y ON x.a = y.a LEFT JOIN sjz z ON x.a = z.q1;
EXPLAIN (COSTS OFF) SELECT * FROM sj x JOIN sj y ON x.a = y.a LEFT JOIN sjz z ON y.a = z.q1;
SELECT * FROM sj x JOIN sj y ON x.a = y.a LEFT JOIN sjz z ON y.a = z.q1 ORDER BY x.a;
-- Uniqueness from constants.
EXPLAIN (COSTS OFF) SELECT * FROM sj j1, sj j2 WHERE j1.b = j2.b AND j1.a = 2 AND j2.a = 3;
SELECT * FROM sj j1, sj j2 WHERE j1.b = j2.b AND j1.a = 2 AND j2.a = 3;
EXPLAIN (COSTS OFF) SELECT * FROM sj j1, sj j2 WHERE j1.b = j2.b AND j1.a = 2 AND j2.a = 2;
SELECT * FROM sj j1, sj j2 WHERE j1.b = j2.b AND j1.a = 2 AND j2.a = 2;
EXPLAIN (COSTS OFF) SELECT * FROM sj j1, sj j2 WHERE j1.b = j2.b AND 1 = j1.a AND j2.a = 1;
-- A primary key, declared NOT NULL: no IS NOT NULL test.
EXPLAIN SELECT s1.w, s2.v FROM sjk s1 JOIN sjk s2 ON s1.id = s2.id WHERE s2.v = 3;
SELECT s1.w, s2.v FROM sjk s1 JOIN sjk s2 ON s1.id = s2.id WHERE s2.v = 3 ORDER BY s1.w LIMIT 5;
EXPLAIN SELECT s1.w FROM sjk s1 JOIN sjk s2 ON s1.id = s2.id WHERE s1.v = 3 AND s2.v = 3;
EXPLAIN SELECT s1.w FROM sjk s1 JOIN sjk s2 ON s1.id = s2.id WHERE s1.v = 3 AND s2.v < 5;
EXPLAIN (VERBOSE, COSTS OFF) SELECT s1.w, s2.v FROM sjk s1 JOIN sjk s2 ON s1.id = s2.id;
EXPLAIN SELECT s1.v, count(*) FROM sjk s1 JOIN sjk s2 ON s1.id = s2.id GROUP BY s1.v ORDER BY s1.v;
SELECT s1.v, count(*) FROM sjk s1 JOIN sjk s2 ON s1.id = s2.id GROUP BY s1.v ORDER BY s1.v;
EXPLAIN SELECT s1.w FROM sjk s1 JOIN sjk s2 ON s1.id = s2.id ORDER BY s1.id LIMIT 3;
-- A third table joined to both scans.
EXPLAIN SELECT o.id, s1.w, s2.v FROM sjo o JOIN sjk s1 ON o.ref = s1.id JOIN sjk s2 ON s2.id = s1.id;
SELECT count(*), sum(s2.v) FROM sjo o JOIN sjk s1 ON o.ref = s1.id JOIN sjk s2 ON s2.id = s1.id;
EXPLAIN SELECT o.id FROM sjo o, sjk s1, sjk s2 WHERE o.ref = s1.id AND s1.id = s2.id AND o.id < s2.v;
EXPLAIN SELECT * FROM sjk s1 JOIN sjk s2 ON s1.id = s2.id AND s1.v < s2.v;
-- Disabled.
SET enable_self_join_elimination = off;
EXPLAIN (COSTS OFF) SELECT p.* FROM sj p, sj q WHERE q.a = p.a;
RESET enable_self_join_elimination;
DROP TABLE sj, sjk, sjo, sjz;

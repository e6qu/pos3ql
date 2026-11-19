-- Self-joins PostgreSQL 18 cannot remove plan through the join search, and
-- removable ones and joined writes return PostgreSQL's rows.
CREATE TABLE sj (id int PRIMARY KEY, k int, v text);
CREATE INDEX sj_k ON sj (k);
INSERT INTO sj SELECT g, g % 50, 'v' || g FROM generate_series(1, 1000) g;
CREATE TABLE so (id int, k int);
INSERT INTO so SELECT g, g % 7 FROM generate_series(1, 300) g;
CREATE TABLE sb (id int PRIMARY KEY, k int);
INSERT INTO sb SELECT g, g % 30 FROM generate_series(1, 600) g;
ANALYZE sj, so, sb;
-- Not removable: no unique key equated, or an outer join between.
EXPLAIN (COSTS OFF) SELECT a.v FROM sj a JOIN sj b ON a.k = b.k WHERE a.id < 3;
EXPLAIN (COSTS OFF) SELECT a.v FROM sj a LEFT JOIN sj b ON a.id = b.id;
SET enable_self_join_elimination = off;
EXPLAIN (COSTS OFF) SELECT a.v, b.k FROM sj a JOIN sj b ON a.id = b.id WHERE b.k = 3;
RESET enable_self_join_elimination;
SELECT count(*), min(a.v) FROM sj a JOIN sj b ON a.id = b.id WHERE b.k = 3;
SELECT count(*) FROM sj a JOIN so ON so.id = a.id JOIN sj b ON b.id = so.id;
BEGIN;
UPDATE sj SET v = 'x' FROM sb WHERE sj.id = sb.id AND sb.k = 3;
SELECT count(*) FROM sj WHERE v = 'x';
ROLLBACK;

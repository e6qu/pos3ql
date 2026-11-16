-- A unique index build over duplicate keys names the first equal pair its
-- in-memory tuplesort compares (comparetup_index_btree_tiebreak), with rows
-- fed in heap order: small sorts, median-of-nine partitions, DESC and NULLS
-- orders, NULLS NOT DISTINCT, expression and partial keys, numerics equal
-- in value but not in text, and rows an UPDATE moved to the heap's end.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/include/lib/sort_template.h
CREATE TABLE u (id integer, a integer, b text, n numeric);
INSERT INTO u SELECT g, (g * 37) % 101, 'v' || ((g * 13) % 50), ((g * 7) % 30)::numeric
FROM generate_series(1, 300) g;
CREATE UNIQUE INDEX u_a ON u (a);
CREATE UNIQUE INDEX u_b ON u (b);
CREATE UNIQUE INDEX u_b_desc ON u (b DESC);
CREATE UNIQUE INDEX u_ab ON u (a, b);
CREATE UNIQUE INDEX u_lower ON u (lower(b));
CREATE UNIQUE INDEX u_part ON u (a) WHERE id > 150;
CREATE UNIQUE INDEX u_n ON u (n);
CREATE TABLE s (a integer, b integer);
INSERT INTO s VALUES (3, 1), (1, 1), (2, NULL), (3, 2), (NULL, 5), (1, NULL);
CREATE UNIQUE INDEX s_a ON s (a);
CREATE UNIQUE INDEX s_b ON s (b);
CREATE UNIQUE INDEX s_b_nnd ON s (b) NULLS NOT DISTINCT;
CREATE UNIQUE INDEX s_ab ON s (a, b);
INSERT INTO s SELECT g % 9, g FROM generate_series(1, 30) g;
CREATE UNIQUE INDEX s_a2 ON s (a DESC NULLS LAST);
CREATE TABLE m (n numeric);
INSERT INTO m VALUES (1.0), (2), (1.00), (3), (2.000), (5), (6), (7), (8);
CREATE UNIQUE INDEX m_n ON m (n);
CREATE TABLE r (a integer UNIQUE, b integer);
CREATE UNIQUE INDEX r_b ON r (b);
INSERT INTO r SELECT g, g FROM generate_series(1, 50) g;
BEGIN;
INSERT INTO r VALUES (51, 51);
CREATE UNIQUE INDEX r_c ON r ((b % 7));
ROLLBACK;
CREATE TABLE w (a integer, b integer);
INSERT INTO w SELECT g % 45, g % 17 FROM generate_series(1, 200) g;
DELETE FROM w WHERE a = 3;
CREATE UNIQUE INDEX w_a ON w (a);
UPDATE w SET b = b + 1 WHERE a < 10;
CREATE UNIQUE INDEX w_b ON w (b);
CREATE TABLE x (a integer);
INSERT INTO x SELECT g FROM generate_series(1, 10) g;
UPDATE x SET a = 7 WHERE a = 1;
UPDATE x SET a = 2 WHERE a = 9;
CREATE UNIQUE INDEX x_a ON x (a);
DROP TABLE u, s, m, r, w, x;

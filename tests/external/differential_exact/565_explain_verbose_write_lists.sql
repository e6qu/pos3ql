-- PostgreSQL 18 EXPLAIN VERBOSE target lists of one-table writes: an
-- INSERT's RETURNING list on its write node, a several-row VALUES list's
-- "*VALUES*" columns, an UPDATE scan's new values coerced to each column,
-- and a RETURNING list naming the old and new rows.
CREATE TABLE t565 (id int PRIMARY KEY, x bigint, s text);
INSERT INTO t565 VALUES (1, 1, 'a'), (2, 2, 'b');
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO t565 VALUES (3, 3, 'c') RETURNING id, s;
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO t565 VALUES (3, 3, 'c'), (4, 4, 'd');
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO t565 AS n VALUES (3, 3, 'c'), (4, 4, 'd') RETURNING n.*;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE t565 SET s = 'z', x = 7 WHERE id = 1;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE t565 SET x = x + 1 WHERE id = 1 RETURNING old.x, new.x;
EXPLAIN (VERBOSE, COSTS OFF) DELETE FROM t565 WHERE id = 2 RETURNING old.*, s;
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO t565 VALUES (3, DEFAULT, 'c');
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO t565 (id, s) VALUES (3, 'c');
DROP TABLE t565;

-- SQL cursors: DECLARE / FETCH / MOVE / CLOSE. Materialized at DECLARE
-- (PostgreSQL's insensitive-cursor snapshot), SCROLL for backward and
-- absolute positioning, WITH HOLD across commit, and the exact motion
-- semantics of every FETCH direction.
DROP TABLE IF EXISTS ct;
CREATE TABLE ct(a int, b text);
INSERT INTO ct SELECT g, 'row-' || g FROM generate_series(1, 10) g;

-- Forward-only basics.
BEGIN;
DECLARE c CURSOR FOR SELECT a, b FROM ct ORDER BY a;
FETCH 3 FROM c;
FETCH NEXT FROM c;
FETCH FROM c;
FETCH ALL FROM c;
FETCH FROM c;
FETCH 0 FROM c;
MOVE BACKWARD 2 IN c;
CLOSE c;
FETCH FROM c;
COMMIT;

-- SCROLL: backward, absolute, relative, first/last/prior, MOVE.
BEGIN;
DECLARE s SCROLL CURSOR FOR SELECT a FROM ct ORDER BY a;
FETCH 12 FROM s;
FETCH BACKWARD 2 FROM s;
FETCH BACKWARD ALL FROM s;
FETCH ABSOLUTE 3 FROM s;
FETCH ABSOLUTE -2 FROM s;
FETCH RELATIVE -1 FROM s;
FETCH RELATIVE 2 FROM s;
FETCH FIRST FROM s;
FETCH LAST FROM s;
FETCH PRIOR FROM s;
MOVE 5 FROM s;
FETCH 0 FROM s;
MOVE ABSOLUTE 4 IN s;
FETCH FORWARD 2 FROM s;
FETCH -3 FROM s;
COMMIT;
FETCH FROM s;

-- The snapshot is taken at DECLARE: later changes are invisible to it.
BEGIN;
DECLARE snap CURSOR FOR SELECT count(*) FROM ct;
INSERT INTO ct VALUES (11, 'row-11');
FETCH FROM snap;
CLOSE snap;
COMMIT;
SELECT count(*) FROM ct;
DELETE FROM ct WHERE a = 11;

-- WITH HOLD survives commit; rollback kills everything created in the block.
BEGIN;
DECLARE h CURSOR WITH HOLD FOR SELECT a FROM ct ORDER BY a DESC;
COMMIT;
FETCH 2 FROM h;
BEGIN;
DECLARE dead CURSOR WITH HOLD FOR SELECT 1;
DECLARE dead2 CURSOR FOR SELECT 1;
ROLLBACK;
FETCH FROM dead;
FETCH FROM dead2;
FETCH 2 FROM h;
CLOSE h;

-- Errors: DECLARE outside a block, duplicates, unknown cursors, backward on
-- a forward-only cursor (which aborts the transaction), CLOSE ALL.
DECLARE nope CURSOR FOR SELECT 1;
BEGIN;
DECLARE twice CURSOR FOR SELECT 1;
DECLARE twice CURSOR FOR SELECT 2;
ROLLBACK;
BEGIN;
DECLARE fwd CURSOR FOR SELECT a FROM ct ORDER BY a;
FETCH BACKWARD 1 FROM fwd;
FETCH FROM fwd;
ROLLBACK;
CLOSE nosuch;
BEGIN;
DECLARE a1 CURSOR FOR SELECT 1;
DECLARE a2 CURSOR FOR SELECT 2;
CLOSE ALL;
FETCH FROM a1;
ROLLBACK;

-- INSENSITIVE / NO SCROLL spellings parse; a cursor over a join works.
BEGIN;
DECLARE j INSENSITIVE NO SCROLL CURSOR WITHOUT HOLD FOR
  SELECT x.a, y.b FROM ct x JOIN ct y ON y.a = x.a WHERE x.a <= 2 ORDER BY x.a;
FETCH ALL FROM j;
COMMIT;

DROP TABLE ct;

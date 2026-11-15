-- postgres_fdw's remote query text: shipped conditions, columns, order and
-- limit, direct and row-by-row UPDATE/DELETE, and connection invalidation by
-- COMMIT PREPARED. Run with -v fdw_host=... -v fdw_port=... -v fdw_user=...
-- naming the reference server, after foreign-pushdown-remote.sql ran there,
-- with PGHOST/PGPORT/PGUSER naming the server under test.
CREATE SERVER fdw_push FOREIGN DATA WRAPPER postgres_fdw
  OPTIONS (host :'fdw_host', port :'fdw_port', dbname 'postgres');
CREATE USER MAPPING FOR CURRENT_USER SERVER fdw_push OPTIONS (user :'fdw_user');
CREATE FOREIGN TABLE ft (id int, s smallint, b bigint, n numeric, f float8, note text,
  v varchar(10), c char(3), d date, ts timestamp, flag boolean,
  odd int OPTIONS (column_name 'Odd Name'), q int)
  SERVER fdw_push OPTIONS (schema_name 'fdw_push_remote', table_name 't');
CREATE FOREIGN TABLE app (name text)
  SERVER fdw_push OPTIONS (schema_name 'fdw_push_remote', table_name 'app');
\set e 'EXPLAIN (VERBOSE, COSTS OFF)'

-- Shipped conditions, in the planner's order: by cost, equalities last.
:e SELECT * FROM ft;
:e SELECT id, note FROM ft WHERE q > 50 AND note <> 'x';
:e SELECT id FROM ft WHERE s = 3 AND b > 5 AND b < 5000000000 AND n = 1.5 AND n > 1 AND f > 0.5 AND f < 2;
:e SELECT id FROM ft WHERE id = -3 OR s <> -2 OR n = -1.25 OR f = -1;
:e SELECT id FROM ft WHERE note = 'a''b' AND v = 'v1' AND c = 'c1' AND note > v;
:e SELECT id FROM ft WHERE d = '2024-01-05' AND ts > '2024-01-01 12:00' AND d < date '2024-02-01';
:e SELECT id FROM ft WHERE flag AND NOT flag OR flag = true OR flag IS NULL OR note IS NOT NULL;
:e SELECT id FROM ft WHERE id + 1 > 3 AND id * 2 < s + b AND n / 2 > id AND note || 'x' = 'n1x' AND id % 2 = 0;
:e SELECT id FROM ft WHERE id IN (1, 2, 3) AND note IN ('a', 'b c', NULL) AND id NOT IN (5, 6) AND b IN (1, 5000000000) AND id IN (4);
:e SELECT id FROM ft WHERE note LIKE 'n%' AND note NOT LIKE 'x%' AND note ILIKE 'N%' AND v LIKE 'v%' AND note ~ '^n' AND note !~* 'Q';
:e SELECT id FROM ft WHERE odd = 3 AND n IN (1.5, 2) AND f IN (0.5, 1);
:e SELECT * FROM ft WHERE upper(note) = 'N1' AND id > 2;
:e SELECT id FROM ft WHERE random() > 0.5;
:e SELECT id FROM ft WHERE ts < now();

-- Order and limit, and what stays local.
:e SELECT id FROM ft ORDER BY q DESC LIMIT 3;
:e SELECT id FROM ft WHERE d < '2024-01-05' ORDER BY id;
:e SELECT id FROM ft WHERE note LIKE 'n1%' OR id IN (1, 2) ORDER BY note NULLS FIRST LIMIT 2 OFFSET 1;
:e SELECT id, note FROM ft ORDER BY n DESC, id;
:e SELECT id, note AS label FROM ft ORDER BY label LIMIT 5;
:e SELECT id FROM ft ORDER BY 1 DESC NULLS LAST OFFSET 2;
:e SELECT id FROM ft WHERE upper(note) <> 'X' ORDER BY id LIMIT 2;
:e SELECT id FROM ft LIMIT 4;
:e SELECT id, s FROM ft WHERE id > 3 FOR UPDATE;
:e SELECT t.id, t.note FROM ft t WHERE t.id < 4 ORDER BY t.note DESC;
SELECT id, note FROM ft WHERE id IN (1, 2, 3) AND note IN ('n1', 'n3') ORDER BY id;
SELECT id, label FROM (SELECT id, note AS label FROM ft ORDER BY label LIMIT 5) s;
SELECT id, s, b, n, f, note, v, c, d, ts, flag, odd FROM ft WHERE id = 7;
SELECT id, q FROM ft WHERE q > 150 ORDER BY id;

-- UPDATE and DELETE: one remote statement when everything ships, else a
-- locking scan and a command per row.
:e UPDATE ft SET q = q + 1 WHERE id = 3;
:e UPDATE ft SET q = q + 1, note = 'x' WHERE id = 3 AND random() < 2;
:e DELETE FROM ft WHERE id = 4;
:e DELETE FROM ft WHERE id = 4 AND random() < 2;
UPDATE ft SET q = 5000 WHERE id = 3;
UPDATE ft SET q = 5000 WHERE id = 3 AND random() < 2;
UPDATE ft SET q = q + 1, note = note || '!' WHERE id = 5 AND random() < 2 RETURNING id, q, note;
UPDATE ft SET q = q + 1 WHERE id IN (6, 7);
DELETE FROM ft WHERE id = 4 RETURNING note;
DELETE FROM ft WHERE id = 8 AND random() < 2;
SELECT id, q, note FROM ft WHERE id BETWEEN 3 AND 8 ORDER BY id;
-- A direct command ships what its RETURNING list reads.
:e UPDATE ft SET q = q + 1 WHERE id = 9 RETURNING id, q, tableoid::regclass;
UPDATE ft SET q = q + 1 WHERE id IN (9, 10) RETURNING id, q, tableoid::regclass;
UPDATE ft SET q = q + 1 WHERE id = 9 RETURNING old.q, new.q;
DELETE FROM ft WHERE id = 11 RETURNING ft;
DELETE FROM ft WHERE id IN (12, 13) RETURNING 1;
WITH w AS (UPDATE ft SET q = q + 1 WHERE id > 15 RETURNING id, q) SELECT * FROM w ORDER BY id;
DO $$ DECLARE r int; BEGIN UPDATE ft SET q = q + 1 WHERE id = 14 RETURNING q INTO r; RAISE NOTICE 'r=%', r; END $$;
UPDATE ft SET q = 2000 WHERE id = 15 RETURNING id;
SELECT id, q FROM ft WHERE id BETWEEN 9 AND 20 ORDER BY id;
-- RETURNING reads the remote row's ctid, which the remote command returns
-- after the columns.
BEGIN;
UPDATE ft SET q = q + 1 WHERE id = 16 RETURNING ctid, id, q;
UPDATE ft SET q = q + 1 WHERE id = 16 AND random() < 2 RETURNING ft.ctid;
DELETE FROM ft WHERE id = 17 RETURNING ctid, id;
DELETE FROM ft WHERE id = 18 AND random() < 2 RETURNING ctid;
INSERT INTO ft (id, q) VALUES (40, 4) RETURNING ctid, id;
ROLLBACK;

-- Grouping and aggregates ship when the whole query does.
:e SELECT s % 3, count(*), sum(b), avg(n), max(note), min(d) FROM ft WHERE id < 15 GROUP BY s % 3 HAVING count(*) > 1 ORDER BY count(*) DESC, 1 LIMIT 2;
SELECT s % 3, count(*), sum(b), avg(n), max(note), min(d) FROM ft WHERE id < 15 GROUP BY s % 3 HAVING count(*) > 1 ORDER BY count(*) DESC, 1 LIMIT 2;
SELECT flag, count(DISTINCT c), sum(f) FILTER (WHERE id > 3), string_agg(v, ',' ORDER BY v DESC) FROM ft GROUP BY flag ORDER BY flag;
SELECT count(*), sum(q), bool_and(flag), every(id > 0) FROM ft;
SELECT count(*) FROM ft WHERE id > 1000;
SELECT id % 4 AS k, count(*) FROM ft GROUP BY k HAVING sum(id) > 30 ORDER BY sum(id);
SELECT count(*) FROM ft GROUP BY note ORDER BY 1 LIMIT 1;
-- A grouped query ships the aggregates of what it computes here: a constant
-- or a star ships, a volatile expression or HAVING condition is evaluated
-- over the remote groups, and an order only this side computes sorts them.
:e SELECT s % 3, 'k', count(*) * 2 + random() * 0, sum(q) FROM ft GROUP BY s % 3 HAVING sum(id) > random() * 0 ORDER BY 1;
SELECT s % 3, 'k', count(*) * 2 + random() * 0, sum(q) FROM ft GROUP BY s % 3 HAVING sum(id) > random() * 0 ORDER BY 1;
SELECT flag, count(*) FROM ft GROUP BY flag HAVING max(id) > 19 + random() * 0;
SELECT c, count(*) FROM ft GROUP BY c ORDER BY count(*) * (1 + random() * 0), c LIMIT 3;
SELECT * FROM app GROUP BY name;

-- The order the plan reads rows in ships: expression keys, DISTINCT's
-- order and a window's.
:e SELECT id, note FROM ft ORDER BY s % 4 DESC, upper(note) LIMIT 5;
SELECT id, note FROM ft ORDER BY s % 4 DESC, upper(note) LIMIT 5;
:e SELECT DISTINCT c, flag FROM ft ORDER BY flag;
SELECT DISTINCT c, flag FROM ft ORDER BY flag;
SELECT DISTINCT ON (flag) flag, id FROM ft ORDER BY flag, id DESC;
SELECT id, sum(q) OVER (PARTITION BY flag ORDER BY id) FROM ft WHERE id < 8;
SELECT max(id), min(note) FROM ft;
-- A lone min or max reads the first row of the remote order.
:e SELECT max(q) + 1 FROM ft WHERE id < 10;
SELECT max(q) + 1 FROM ft WHERE id < 10;
SELECT min(note), min(note) || '!' FROM ft WHERE random() < 2;
SELECT max(id) FROM ft WHERE id > 1000;

-- InitPlan values and prepared statements' parameters ship as $n.
CREATE TABLE fdw_push_local (x int, t text);
INSERT INTO fdw_push_local VALUES (3, 'n4'), (5, 'n7');
:e SELECT id, note FROM ft WHERE (SELECT max(x) FROM fdw_push_local) = id OR note = (SELECT min(t) FROM fdw_push_local);
SELECT id, note FROM ft WHERE (SELECT max(x) FROM fdw_push_local) = id OR note = (SELECT min(t) FROM fdw_push_local) ORDER BY id;
SELECT id FROM ft WHERE id < (SELECT max(x) FROM fdw_push_local) AND b > (SELECT min(x) FROM fdw_push_local) * 1000000000::bigint ORDER BY id;
SELECT id FROM ft WHERE id IN ((SELECT 1), (SELECT 2)) ORDER BY id;
SELECT id FROM ft WHERE id > (SELECT max(id) - 3 FROM ft) ORDER BY id;
SELECT id FROM ft WHERE (SELECT x FROM fdw_push_local ORDER BY x LIMIT 1) > 2 AND id < 3 ORDER BY id;
SELECT id, (SELECT 1) FROM ft ORDER BY id LIMIT 2;
SELECT id FROM ft ORDER BY id LIMIT (SELECT 2);
PREPARE fdw_p(int, text) AS SELECT id, note FROM ft WHERE id = $1 OR note = $2 ORDER BY id;
EXECUTE fdw_p(3, 'n5');
EXECUTE fdw_p(NULL, 'n6');
SET plan_cache_mode = force_generic_plan;
:e EXECUTE fdw_p(3, 'n5');
EXECUTE fdw_p(3, 'n5');
PREPARE fdw_q(date, float8, int, int) AS SELECT id FROM ft WHERE d > $1 AND f < $2 AND id IN ($3, $4) ORDER BY id;
EXECUTE fdw_q('2024-01-02', 1.5, 3, 9);
RESET plan_cache_mode;
DEALLOCATE fdw_p;
DEALLOCATE fdw_q;
DROP TABLE fdw_push_local;

-- Values sent to the server print in postgres_fdw's transmission modes
-- (ISO dates), whatever the session's DateStyle.
SET DateStyle = 'SQL, DMY';
INSERT INTO ft (id, d, ts, f) VALUES (100, '2024-01-05', '2024-01-02 03:04:05', 0.1234567890123);
UPDATE ft SET d = '2024-02-07' WHERE id = 100 AND random() < 2;
COPY ft (id, d) FROM STDIN;
101	2024-01-05
\.
SELECT id, d, ts FROM ft WHERE id >= 100 ORDER BY id;
RESET DateStyle;
SELECT id, d, ts, f FROM ft WHERE id >= 100 ORDER BY id;
DELETE FROM ft WHERE id >= 100;

-- A server change committed by COMMIT PREPARED reaches every client's
-- cached connection, as a plain COMMIT's does.
SELECT name FROM app;
\! psql -X -q -c "BEGIN" -c "ALTER SERVER fdw_push OPTIONS (ADD application_name 'fdw_prepared')" -c "PREPARE TRANSACTION 'fdw_push'" -c "COMMIT PREPARED 'fdw_push'"
SELECT name FROM app;

DROP SERVER fdw_push CASCADE;

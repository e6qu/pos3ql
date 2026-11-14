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

-- A server change committed by COMMIT PREPARED reaches every client's
-- cached connection, as a plain COMMIT's does.
SELECT name FROM app;
\! psql -X -q -c "BEGIN" -c "ALTER SERVER fdw_push OPTIONS (ADD application_name 'fdw_prepared')" -c "PREPARE TRANSACTION 'fdw_push'" -c "COMMIT PREPARED 'fdw_push'"
SELECT name FROM app;

DROP SERVER fdw_push CASCADE;

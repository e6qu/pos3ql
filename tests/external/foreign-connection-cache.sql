-- postgres_fdw's connection cache, cursor scans, INSERT text and batches.
-- Run with -v fdw_host=... -v fdw_port=... naming the reference server,
-- after foreign-connection-cache-remote.sql ran there.
CREATE SERVER fdw_cache FOREIGN DATA WRAPPER postgres_fdw
  OPTIONS (host :'fdw_host', port :'fdw_port', dbname 'postgres');
CREATE USER MAPPING FOR CURRENT_USER SERVER fdw_cache OPTIONS (user :'fdw_user');
CREATE SCHEMA fdw_cache_local;
CREATE FOREIGN TABLE fdw_cache_local.t (id integer, note text,
  g integer GENERATED ALWAYS AS (id * 3) STORED)
  SERVER fdw_cache OPTIONS (schema_name 'fdw_cache_remote', table_name 't');
CREATE FOREIGN TABLE fdw_cache_local.d (id integer)
  SERVER fdw_cache OPTIONS (schema_name 'fdw_cache_remote', table_name 'd');
CREATE FOREIGN TABLE fdw_cache_local.b (id integer, note text)
  SERVER fdw_cache OPTIONS (schema_name 'fdw_cache_remote', table_name 'b', batch_size '3');
CREATE FOREIGN TABLE fdw_cache_local.calls (n integer)
  SERVER fdw_cache OPTIONS (schema_name 'fdw_cache_remote', table_name 'calls');
CREATE FOREIGN TABLE fdw_cache_local.bad (id integer, q integer)
  SERVER fdw_cache OPTIONS (schema_name 'fdw_cache_remote', table_name 'bad');
CREATE FOREIGN TABLE fdw_cache_local.v (id integer, n bigint)
  SERVER fdw_cache OPTIONS (schema_name 'fdw_cache_remote', table_name 'v', fetch_size '6');

-- The cache: one connection per user mapping, kept across transactions.
SELECT server_name, valid FROM postgres_fdw_get_connections() ORDER BY 1;
SELECT count(*), sum(id) FROM fdw_cache_local.t;
SELECT server_name, user_name = current_user AS mapped, valid, used_in_xact, closed
  FROM postgres_fdw_get_connections() ORDER BY 1;
BEGIN;
SELECT count(*) FROM fdw_cache_local.t WHERE id < 3;
SELECT server_name, valid, used_in_xact FROM postgres_fdw_get_connections() ORDER BY 1;
SELECT postgres_fdw_disconnect('fdw_cache');
ALTER SERVER fdw_cache OPTIONS (ADD fetch_size '7');
SELECT server_name, valid, used_in_xact FROM postgres_fdw_get_connections() ORDER BY 1;
COMMIT;
SELECT server_name, valid, used_in_xact FROM postgres_fdw_get_connections() ORDER BY 1;
SELECT count(*) FROM fdw_cache_local.t;
SELECT postgres_fdw_disconnect('fdw_cache');
SELECT postgres_fdw_disconnect('fdw_cache');
SELECT postgres_fdw_disconnect_all();
SELECT postgres_fdw_disconnect('no_such_server');
ALTER SERVER fdw_cache OPTIONS (ADD keep_connections 'off');
SELECT count(*) FROM fdw_cache_local.t;
SELECT server_name FROM postgres_fdw_get_connections();
ALTER SERVER fdw_cache OPTIONS (SET keep_connections 'on');
ALTER USER MAPPING FOR CURRENT_USER SERVER fdw_cache OPTIONS (SET user :'fdw_user');
SELECT count(*) FROM fdw_cache_local.t;
BEGIN;
SELECT count(*) FROM fdw_cache_local.d;
ALTER USER MAPPING FOR CURRENT_USER SERVER fdw_cache OPTIONS (SET user :'fdw_user');
SELECT server_name, valid, used_in_xact FROM postgres_fdw_get_connections() ORDER BY 1;
ROLLBACK;
SELECT server_name, valid, used_in_xact FROM postgres_fdw_get_connections() ORDER BY 1;

-- Remote commands as postgres_fdw spells them, in error context.
SELECT * FROM fdw_cache_local.bad;
INSERT INTO fdw_cache_local.t (id, note) VALUES (1, 'duplicate');
INSERT INTO fdw_cache_local.t (id, note) VALUES (100, 'a') RETURNING note;
INSERT INTO fdw_cache_local.t (id, note) VALUES (101, 'b') RETURNING id, note, g;
INSERT INTO fdw_cache_local.t (id, note) VALUES (102, 'c') RETURNING fdw_cache_local.t;
INSERT INTO fdw_cache_local.t (id, note) VALUES (103, 'd'), (1, 'duplicate');
INSERT INTO fdw_cache_local.t (id, note) VALUES (1, 'skipped') ON CONFLICT DO NOTHING;
INSERT INTO fdw_cache_local.t (id, note) VALUES (104, 'e') ON CONFLICT DO NOTHING RETURNING id;
SELECT id, note, g FROM fdw_cache_local.t WHERE id > 99 ORDER BY id;

-- fetch_size: a table's option over its server's; each FETCH reads a batch.
ALTER SERVER fdw_cache OPTIONS (SET fetch_size 'x');
ALTER SERVER fdw_cache OPTIONS (SET fetch_size '0');
ALTER SERVER fdw_cache OPTIONS (SET fetch_size '1.6');
ALTER SERVER fdw_cache OPTIONS (SET keep_connections 'yes');
SELECT count(*) FROM fdw_cache_local.t;
SELECT count(*), max(n) FROM fdw_cache_local.v;

-- batch_size: one remote INSERT per batch, counted by its statement trigger.
INSERT INTO fdw_cache_local.b SELECT i, 'b' || i FROM generate_series(1, 7) i;
SELECT count(*) FROM fdw_cache_local.calls;
INSERT INTO fdw_cache_local.b VALUES (1, 'x'), (2, 'y') RETURNING id;
SELECT count(*) FROM fdw_cache_local.calls;
ALTER FOREIGN TABLE fdw_cache_local.t OPTIONS (ADD batch_size '0x10');
INSERT INTO fdw_cache_local.t (id, note) VALUES (200, 'f'), (1, 'skipped'), (201, 'g')
  ON CONFLICT DO NOTHING;
INSERT INTO fdw_cache_local.t (id, note) VALUES (202, 'h'), (1, 'duplicate');

-- A remote error leaves the remote transaction to the local savepoint.
BEGIN;
SAVEPOINT before_error;
INSERT INTO fdw_cache_local.t (id, note) VALUES (1, 'duplicate');
ROLLBACK TO SAVEPOINT before_error;
INSERT INTO fdw_cache_local.t (id, note) VALUES (300, 'kept');
COMMIT;
SELECT id, note FROM fdw_cache_local.t WHERE id >= 200 ORDER BY id;

-- A failed remote COMMIT discards its connection.
BEGIN;
INSERT INTO fdw_cache_local.d VALUES (1), (1);
COMMIT;
SELECT server_name, valid, used_in_xact FROM postgres_fdw_get_connections() ORDER BY 1;
BEGIN;
SELECT count(*) FROM fdw_cache_local.d;
PREPARE TRANSACTION 'fdw_cache';
PREPARE TRANSACTION 'fdw_cache';

-- A server dropped while its connection is in use is shown without a name.
BEGIN;
SELECT count(*) FROM fdw_cache_local.d;
DROP SERVER fdw_cache CASCADE;
SELECT server_name, user_name, valid, used_in_xact FROM postgres_fdw_get_connections() ORDER BY 1;
SELECT postgres_fdw_disconnect_all();
ROLLBACK;
SELECT server_name, valid, used_in_xact FROM postgres_fdw_get_connections() ORDER BY 1;

SELECT postgres_fdw_validator(ARRAY['fetch_size=10', 'keep_connections=on'], 'pg_foreign_server'::regclass);
SELECT postgres_fdw_validator(ARRAY['batch_size=0'], 'pg_foreign_table'::regclass);
SELECT postgres_fdw_validator(ARRAY['keep_connections=on'], 'pg_foreign_table'::regclass);
DROP SCHEMA fdw_cache_local CASCADE;
DROP SERVER fdw_cache CASCADE;

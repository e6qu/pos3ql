-- Remote tables for foreign-connection-cache.sql, created on the reference
-- server, which both PostgreSQL's postgres_fdw loopback and pos3ql reach.
DROP SCHEMA IF EXISTS fdw_cache_remote CASCADE;
CREATE SCHEMA fdw_cache_remote;
CREATE TABLE fdw_cache_remote.t (id integer PRIMARY KEY, note text,
  g integer GENERATED ALWAYS AS (id * 2) STORED);
INSERT INTO fdw_cache_remote.t (id, note) SELECT i, 'n' || i FROM generate_series(1, 25) i;
CREATE TABLE fdw_cache_remote.d (id integer UNIQUE DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE fdw_cache_remote.calls (n integer);
CREATE FUNCTION fdw_cache_remote.count_statement() RETURNS trigger LANGUAGE plpgsql
  AS $$ BEGIN INSERT INTO fdw_cache_remote.calls VALUES (1); RETURN NULL; END $$;
CREATE TABLE fdw_cache_remote.b (id integer, note text);
CREATE TRIGGER b_statement AFTER INSERT ON fdw_cache_remote.b
  FOR EACH STATEMENT EXECUTE FUNCTION fdw_cache_remote.count_statement();
CREATE VIEW fdw_cache_remote.bad AS SELECT id, 10 / (id - 3) AS q FROM fdw_cache_remote.t WHERE id < 6;
CREATE SEQUENCE fdw_cache_remote.s;
CREATE VIEW fdw_cache_remote.v AS SELECT id, nextval('fdw_cache_remote.s') AS n FROM fdw_cache_remote.t;

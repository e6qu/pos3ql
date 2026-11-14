-- Remote tables for foreign-pushdown.sql, created on the reference server,
-- which both PostgreSQL's postgres_fdw loopback and pos3ql reach.
DROP SCHEMA IF EXISTS fdw_push_remote CASCADE;
CREATE SCHEMA fdw_push_remote;
CREATE TABLE fdw_push_remote.t (id int PRIMARY KEY, s smallint, b bigint, n numeric,
  f float8, note text, v varchar(10), c char(3), d date, ts timestamp, flag boolean,
  "Odd Name" int, q int CHECK (q < 1000));
INSERT INTO fdw_push_remote.t SELECT i, i, i * 1000000000::bigint, i / 4.0, i / 8.0, 'n' || i,
  'v' || i, 'c' || (i % 10), date '2024-01-01' + i, timestamp '2024-01-01 10:00' + i * interval '1 hour',
  i % 2 = 0, i, i * 10 FROM generate_series(1, 20) i;
CREATE VIEW fdw_push_remote.app AS SELECT current_setting('application_name') AS name;

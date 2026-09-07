-- PostgreSQL relation persistence is executable catalog and lifetime state,
-- not decoration on CREATE.  This file covers one-session SQL behavior; the
-- driver and recovery probes cover cross-session isolation and process loss.

DROP TABLE IF EXISTS public.relation_persistence_shadow CASCADE;
DROP TABLE IF EXISTS public.relation_persistence_unlogged CASCADE;
DROP TABLE IF EXISTS public.relation_persistence_unlogged_as CASCADE;
DROP SEQUENCE IF EXISTS public.relation_persistence_unlogged_sequence CASCADE;

CREATE TABLE public.relation_persistence_shadow (id integer);
INSERT INTO public.relation_persistence_shadow VALUES (1);
CREATE TEMP TABLE relation_persistence_shadow (id integer) ON COMMIT PRESERVE ROWS;
INSERT INTO relation_persistence_shadow VALUES (2);
SELECT id FROM relation_persistence_shadow;
SELECT id FROM public.relation_persistence_shadow;
SELECT relpersistence
  FROM pg_class
 WHERE oid IN ('relation_persistence_shadow'::regclass,
               'public.relation_persistence_shadow'::regclass)
 ORDER BY relpersistence;

CREATE INDEX relation_persistence_shadow_idx ON relation_persistence_shadow (id);
SELECT table_class.relpersistence, index_class.relpersistence
  FROM pg_index index_catalog
  JOIN pg_class index_class ON index_class.oid = index_catalog.indexrelid
  JOIN pg_class table_class ON table_class.oid = index_catalog.indrelid
 WHERE index_class.relname = 'relation_persistence_shadow_idx';

BEGIN;
CREATE TEMP TABLE relation_persistence_delete (id integer) ON COMMIT DELETE ROWS;
INSERT INTO relation_persistence_delete VALUES (1), (2);
SELECT count(*) FROM relation_persistence_delete;
COMMIT;
SELECT count(*) FROM relation_persistence_delete;

BEGIN;
CREATE TEMP TABLE relation_persistence_drop (id integer) ON COMMIT DROP;
INSERT INTO relation_persistence_drop VALUES (1);
COMMIT;
SELECT count(*) = 0
  FROM pg_class
 WHERE relname = 'relation_persistence_drop';

BEGIN;
CREATE TEMP TABLE relation_persistence_rollback (id integer);
ROLLBACK;
SELECT count(*) = 0
  FROM pg_class
 WHERE relname = 'relation_persistence_rollback';

CREATE TEMP TABLE relation_persistence_temp_as AS
  SELECT id, id::text AS label FROM relation_persistence_shadow;
SELECT relpersistence, count(*)
  FROM pg_class, relation_persistence_temp_as
 WHERE pg_class.oid = 'relation_persistence_temp_as'::regclass
 GROUP BY relpersistence;

BEGIN;
CREATE TEMP TABLE relation_persistence_ctas_delete ON COMMIT DELETE ROWS AS
  SELECT 8 AS id;
CREATE TEMP TABLE relation_persistence_ctas_drop ON COMMIT DROP AS
  SELECT 9 AS id;
COMMIT;
SELECT count(*) FROM relation_persistence_ctas_delete;
SELECT count(*) FROM pg_class WHERE relname = 'relation_persistence_ctas_drop';

SELECT 7 AS id INTO TEMP relation_persistence_select_into;
SELECT relpersistence, id
  FROM pg_class, relation_persistence_select_into
 WHERE pg_class.oid = 'relation_persistence_select_into'::regclass;

CREATE TEMP SEQUENCE relation_persistence_temp_sequence START 10;
SELECT relpersistence
  FROM pg_class
 WHERE oid = 'relation_persistence_temp_sequence'::regclass;
SELECT nextval('relation_persistence_temp_sequence');
ALTER SEQUENCE relation_persistence_temp_sequence SET LOGGED;

CREATE GLOBAL TEMPORARY TABLE relation_persistence_global_temp (id integer);
CREATE LOCAL TEMP SEQUENCE relation_persistence_local_temp_sequence;
SELECT relname, relpersistence
  FROM pg_class
 WHERE relname IN (
   'relation_persistence_global_temp',
   'relation_persistence_local_temp_sequence'
 )
 ORDER BY relname;

CREATE TEMP TABLE relation_persistence_temp_fk_parent (id integer PRIMARY KEY);
CREATE TABLE relation_persistence_permanent_fk_child (
  id integer REFERENCES relation_persistence_temp_fk_parent
);
CREATE TABLE relation_persistence_permanent_fk_parent (id integer PRIMARY KEY);
CREATE TEMP TABLE relation_persistence_temp_fk_child (
  id integer REFERENCES relation_persistence_permanent_fk_parent
);
CREATE UNLOGGED TABLE relation_persistence_unlogged_fk_child (
  id integer REFERENCES relation_persistence_permanent_fk_parent
);
CREATE VIEW public.relation_persistence_temp_view AS
  SELECT id FROM relation_persistence_shadow;
CREATE MATERIALIZED VIEW public.relation_persistence_temp_matview AS
  SELECT id FROM relation_persistence_shadow;
CREATE UNLOGGED MATERIALIZED VIEW public.relation_persistence_unlogged_matview AS
  SELECT id FROM relation_persistence_shadow;

CREATE UNLOGGED TABLE public.relation_persistence_unlogged (
  id integer GENERATED ALWAYS AS IDENTITY,
  payload text
);
CREATE INDEX relation_persistence_unlogged_payload_idx
  ON public.relation_persistence_unlogged (payload);
INSERT INTO public.relation_persistence_unlogged (payload)
VALUES ('one'), ('two');
SELECT id, payload FROM public.relation_persistence_unlogged ORDER BY id;
SELECT relname, relpersistence
  FROM pg_class
 WHERE relname IN (
   'relation_persistence_unlogged',
   'relation_persistence_unlogged_id_seq',
   'relation_persistence_unlogged_payload_idx'
 )
 ORDER BY relname;

ALTER TABLE public.relation_persistence_unlogged SET LOGGED;
SELECT relname, relpersistence
  FROM pg_class
 WHERE relname IN (
   'relation_persistence_unlogged',
   'relation_persistence_unlogged_id_seq',
   'relation_persistence_unlogged_payload_idx'
 )
 ORDER BY relname;
SELECT id, payload FROM public.relation_persistence_unlogged ORDER BY id;

ALTER TABLE public.relation_persistence_unlogged SET UNLOGGED;
SELECT relname, relpersistence
  FROM pg_class
 WHERE relname IN (
   'relation_persistence_unlogged',
   'relation_persistence_unlogged_id_seq',
   'relation_persistence_unlogged_payload_idx'
 )
 ORDER BY relname;
CREATE PUBLICATION relation_persistence_temp_publication
  FOR TABLE relation_persistence_shadow;
CREATE PUBLICATION relation_persistence_unlogged_publication
  FOR TABLE public.relation_persistence_unlogged;

CREATE UNLOGGED TABLE public.relation_persistence_unlogged_as AS
  SELECT id, payload FROM public.relation_persistence_unlogged;
SELECT relpersistence, count(*)
  FROM pg_class, public.relation_persistence_unlogged_as
 WHERE pg_class.oid = 'public.relation_persistence_unlogged_as'::regclass
 GROUP BY relpersistence;

CREATE UNLOGGED SEQUENCE public.relation_persistence_unlogged_sequence START 20;
SELECT relpersistence
  FROM pg_class
 WHERE oid = 'public.relation_persistence_unlogged_sequence'::regclass;
SELECT nextval('public.relation_persistence_unlogged_sequence');
ALTER SEQUENCE public.relation_persistence_unlogged_sequence SET LOGGED;
SELECT relpersistence
  FROM pg_class
 WHERE oid = 'public.relation_persistence_unlogged_sequence'::regclass;
ALTER SEQUENCE public.relation_persistence_unlogged_sequence SET UNLOGGED;

CREATE UNLOGGED TABLE relation_persistence_partitioned (id integer)
  PARTITION BY RANGE (id);

CREATE TABLE relation_persistence_inheritance_parent (id integer);
CREATE TEMP TABLE relation_persistence_inheritance_child (label text)
  INHERITS (relation_persistence_inheritance_parent);
INSERT INTO relation_persistence_inheritance_child (id) VALUES (17);
SELECT id FROM relation_persistence_inheritance_parent ORDER BY id;

CREATE TEMP TABLE relation_persistence_temporary_parent (id integer);
CREATE TABLE relation_persistence_permanent_child (label text)
  INHERITS (relation_persistence_temporary_parent);

CREATE TABLE relation_persistence_partition_parent (id integer)
  PARTITION BY RANGE (id);
CREATE UNLOGGED TABLE relation_persistence_unlogged_partition
  PARTITION OF relation_persistence_partition_parent
  FOR VALUES FROM (0) TO (10);
INSERT INTO relation_persistence_partition_parent VALUES (4);
SELECT id FROM relation_persistence_partition_parent;
SELECT relpersistence
  FROM pg_class
 WHERE oid = 'relation_persistence_unlogged_partition'::regclass;
ALTER TABLE relation_persistence_unlogged_partition SET LOGGED;
ALTER TABLE relation_persistence_unlogged_partition SET UNLOGGED;

CREATE TABLE relation_persistence_logged_parent (id integer PRIMARY KEY);
CREATE TABLE relation_persistence_logged_child (
  id integer REFERENCES relation_persistence_logged_parent
);
ALTER TABLE relation_persistence_logged_parent SET UNLOGGED;
ALTER TABLE relation_persistence_logged_child SET UNLOGGED;
ALTER TABLE relation_persistence_logged_parent SET UNLOGGED;
ALTER TABLE relation_persistence_logged_child SET LOGGED;
SELECT relname, relpersistence
  FROM pg_class
 WHERE relname IN (
   'relation_persistence_logged_parent',
   'relation_persistence_logged_child'
 )
 ORDER BY relname;

CREATE TEMP TABLE relation_persistence_temporary_partition_parent (id integer)
  PARTITION BY RANGE (id);
CREATE TEMP TABLE relation_persistence_temporary_partition
  PARTITION OF relation_persistence_temporary_partition_parent
  FOR VALUES FROM (0) TO (10);
SELECT relname, relpersistence
  FROM pg_class
 WHERE relname IN (
   'relation_persistence_temporary_partition_parent',
   'relation_persistence_temporary_partition'
 )
 ORDER BY relname;

CREATE TABLE relation_persistence_permanent_on_commit (id integer)
  ON COMMIT DROP;

DROP TABLE relation_persistence_temp_as;
DROP TABLE relation_persistence_select_into;
DROP TABLE relation_persistence_delete;
DROP TABLE relation_persistence_shadow;
DROP TABLE relation_persistence_inheritance_parent CASCADE;
DROP TABLE relation_persistence_partition_parent CASCADE;
DROP TABLE relation_persistence_logged_parent CASCADE;
DROP TABLE relation_persistence_permanent_fk_parent CASCADE;
DROP SEQUENCE relation_persistence_temp_sequence;
DROP TABLE public.relation_persistence_unlogged_as;
DROP TABLE public.relation_persistence_unlogged;
DROP SEQUENCE public.relation_persistence_unlogged_sequence;
DROP TABLE public.relation_persistence_shadow;

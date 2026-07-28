-- Durable stored-query dependencies: PostgreSQL binds views and materialized
-- views to catalog identities, not names. Exercise rename/schema-move
-- survival, transitive CASCADE, RESTRICT, sequence dependencies, and the
-- transaction characteristics accepted by pos3ql.
DROP SCHEMA IF EXISTS dependency_source CASCADE;
DROP SCHEMA IF EXISTS dependency_consumer CASCADE;

BEGIN ISOLATION LEVEL READ COMMITTED, READ WRITE, NOT DEFERRABLE;
ROLLBACK;

CREATE SCHEMA dependency_source;
CREATE SCHEMA dependency_consumer;
CREATE TYPE dependency_source.mood AS ENUM ('one');
CREATE TABLE dependency_source.items (id integer);
INSERT INTO dependency_source.items VALUES (7);
CREATE VIEW dependency_consumer.items AS
  SELECT id, 'one'::dependency_source.mood AS mood
  FROM dependency_source.items;
CREATE MATERIALIZED VIEW dependency_consumer.items_materialized AS
  SELECT id, mood FROM dependency_consumer.items WITH NO DATA;
CREATE VIEW dependency_consumer.items_leaf AS
  SELECT id, mood FROM dependency_consumer.items_materialized;

DROP TABLE dependency_source.items;
ALTER TABLE dependency_source.items RENAME TO renamed_items;
ALTER TYPE dependency_source.mood RENAME TO feeling;
SELECT id, mood::text FROM dependency_consumer.items;

DROP TABLE dependency_source.renamed_items CASCADE;
SELECT count(*) FROM pg_views
 WHERE schemaname = 'dependency_consumer'
   AND viewname IN ('items', 'items_leaf');
SELECT count(*) FROM pg_matviews
 WHERE schemaname = 'dependency_consumer'
   AND matviewname = 'items_materialized';
DROP TYPE dependency_source.feeling;

CREATE SEQUENCE dependency_source.ids;
CREATE VIEW dependency_consumer.sequence_value AS
  SELECT nextval('dependency_source.ids') AS id;
DROP SEQUENCE dependency_source.ids;
DROP SEQUENCE dependency_source.ids CASCADE;
SELECT count(*) FROM pg_views
 WHERE schemaname = 'dependency_consumer'
   AND viewname = 'sequence_value';

DROP SCHEMA dependency_source, dependency_consumer CASCADE;

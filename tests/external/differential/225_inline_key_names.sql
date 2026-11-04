CREATE SCHEMA inline_key_names;
CREATE TABLE inline_key_names.ppppppppppppppppppppppppppppppppppppppppppppppppppppppppppppppp(id integer PRIMARY KEY);
CREATE TABLE inline_key_names.tttttttttttttttttttttttttttttttttttttttt(cccccccccccccccccccccccccccccccccccccccc integer UNIQUE);
CREATE TABLE inline_key_names."ééééééééééééééééééééééééééééééé"("界界界界界界界界界界界界界界界界界界界界界" integer UNIQUE);
CREATE TABLE inline_key_names.a_b(padding integer, c integer UNIQUE);
CREATE TABLE inline_key_names.a(b_c integer UNIQUE);

SELECT 'long-primary', c.conname, octet_length(c.conname::text), i.relname = c.conname
  FROM pg_constraint c JOIN pg_class i ON i.oid = c.conindid
 WHERE c.conrelid = 'inline_key_names.ppppppppppppppppppppppppppppppppppppppppppppppppppppppppppppppp'::regclass AND c.contype = 'p';
SELECT 'balanced-unique', c.conname, octet_length(c.conname::text), i.relname = c.conname
  FROM pg_constraint c JOIN pg_class i ON i.oid = c.conindid
 WHERE c.conrelid = 'inline_key_names.tttttttttttttttttttttttttttttttttttttttt'::regclass AND c.contype = 'u';
SELECT 'unicode-unique', c.conname, octet_length(c.conname::text), i.relname = c.conname
  FROM pg_constraint c JOIN pg_class i ON i.oid = c.conindid
 WHERE c.conrelid = 'inline_key_names."ééééééééééééééééééééééééééééééé"'::regclass AND c.contype = 'u';
SELECT 'namespace-collision', t.relname, c.conname, i.relname = c.conname
  FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid
  JOIN pg_class i ON i.oid = c.conindid JOIN pg_namespace n ON n.oid = t.relnamespace
 WHERE n.nspname = 'inline_key_names' AND t.relname IN ('a_b','a') AND c.contype = 'u'
 ORDER BY t.relname;

CREATE TABLE inline_key_names.before_names AS
  SELECT c.oid AS constraint_oid, c.conindid AS index_oid, c.conname
    FROM pg_constraint c JOIN pg_namespace n ON n.oid = c.connamespace
   WHERE n.nspname = 'inline_key_names' AND c.contype IN ('p','u');
ALTER TABLE inline_key_names.a_b RENAME COLUMN c TO retained_column;
ALTER TABLE inline_key_names.a_b RENAME TO renamed_ab;
ALTER TABLE inline_key_names.renamed_ab DROP COLUMN padding;
SELECT 'retained-after-renames', c.conname, i.relname = c.conname, c.conindid = b.index_oid
  FROM pg_constraint c JOIN pg_class i ON i.oid = c.conindid
  JOIN inline_key_names.before_names b ON b.constraint_oid = c.oid
 WHERE c.conrelid = 'inline_key_names.renamed_ab'::regclass AND c.contype = 'u';

REINDEX TABLE CONCURRENTLY inline_key_names.ppppppppppppppppppppppppppppppppppppppppppppppppppppppppppppppp;
REINDEX TABLE CONCURRENTLY inline_key_names.tttttttttttttttttttttttttttttttttttttttt;
REINDEX TABLE CONCURRENTLY inline_key_names."ééééééééééééééééééééééééééééééé";
REINDEX TABLE CONCURRENTLY inline_key_names.renamed_ab;
REINDEX TABLE CONCURRENTLY inline_key_names.a;
SELECT 'after-reindex', count(*) = 5, bool_and(c.oid = b.constraint_oid),
       bool_and(c.conname = b.conname), bool_and(c.conindid <> b.index_oid),
       bool_and(i.relname = c.conname)
  FROM pg_constraint c JOIN inline_key_names.before_names b ON b.constraint_oid = c.oid
  JOIN pg_class i ON i.oid = c.conindid;

ALTER TABLE inline_key_names.renamed_ab DROP CONSTRAINT a_b_c_key;
ALTER TABLE inline_key_names.a DROP CONSTRAINT a_b_c_key1;
SELECT 'dropped-by-retained-name', count(*) = 0
  FROM pg_constraint WHERE conrelid IN ('inline_key_names.renamed_ab'::regclass,
                                      'inline_key_names.a'::regclass) AND contype = 'u';
SELECT 'backing-relations-removed', count(*) = 0 FROM pg_class c
  JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE n.nspname = 'inline_key_names' AND c.relname IN ('a_b_c_key','a_b_c_key1');
DROP TABLE inline_key_names.ppppppppppppppppppppppppppppppppppppppppppppppppppppppppppppppp, inline_key_names.tttttttttttttttttttttttttttttttttttttttt,
  inline_key_names."ééééééééééééééééééééééééééééééé", inline_key_names.renamed_ab,
  inline_key_names.a, inline_key_names.before_names;
DROP SCHEMA inline_key_names;

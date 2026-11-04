CREATE SCHEMA generated_key_names;
CREATE TABLE generated_key_names.parent(x integer, y integer, PRIMARY KEY(x,y));
CREATE TABLE generated_key_names.multi_x_y_key(occupied integer);
CREATE TABLE generated_key_names.multi(x integer, y integer, UNIQUE(x,y));
CREATE TABLE generated_key_names.check_holder(v integer CONSTRAINT deferred_v_key CHECK(v > 0));
CREATE TABLE generated_key_names.deferred(v integer UNIQUE DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE generated_key_names.ranges_a_b_excl(occupied integer);
CREATE TABLE generated_key_names.ranges(a int4range, b int4range, EXCLUDE USING gist(a WITH &&, b WITH &&));
CREATE TABLE generated_key_names.child_x_y_fkey(occupied integer);
CREATE TABLE generated_key_names.child(x integer, y integer, FOREIGN KEY(x,y) REFERENCES generated_key_names.parent(x,y));
CREATE TABLE generated_key_names.next_holder(v integer CONSTRAINT next_x_y_fkey CHECK(v > 0));
CREATE TABLE generated_key_names.next(x integer, y integer, FOREIGN KEY(x,y) REFERENCES generated_key_names.parent(x,y));
CREATE DOMAIN generated_key_names.holder AS integer CONSTRAINT third_x_y_fkey CHECK(VALUE > 0);
CREATE TABLE generated_key_names.third(x integer, y integer, FOREIGN KEY(x,y) REFERENCES generated_key_names.parent(x,y));
CREATE TABLE generated_key_names.source(a integer, b integer, UNIQUE(a,b), CONSTRAINT source_positive CHECK(a > 0));
CREATE TABLE generated_key_names.copied_a_b_key(occupied integer);
CREATE TABLE generated_key_names.copied(LIKE generated_key_names.source INCLUDING CONSTRAINTS INCLUDING INDEXES);
CREATE TABLE generated_key_names.partitioned(x integer, y integer, UNIQUE(x,y)) PARTITION BY RANGE(x);
CREATE TABLE generated_key_names.leaf_x_y_key(occupied integer);
CREATE TABLE generated_key_names.leaf PARTITION OF generated_key_names.partitioned FOR VALUES FROM(0) TO(10);
CREATE TABLE generated_key_names.mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm(cccccccccccccccccccccccccccccc integer, dddddddddddddddddddddddddddddd integer, UNIQUE(cccccccccccccccccccccccccccccc,dddddddddddddddddddddddddddddd));
CREATE TABLE generated_key_names."éééééééééééééééééééééééééééééé"("界界界界界界界界界界界界界界界界界界界界" integer, "漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢" integer, UNIQUE("界界界界界界界界界界界界界界界界界界界界","漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢"));
SELECT t.relname, c.contype, c.conname, CASE WHEN c.contype IN ('u','p','x') THEN i.relname = c.conname ELSE c.conindid <> 0 END
 FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid
 LEFT JOIN pg_class i ON i.oid = c.conindid
 JOIN pg_namespace n ON n.oid = c.connamespace
 WHERE n.nspname = 'generated_key_names' AND t.relname IN ('multi','deferred','ranges','child','next','third','copied','leaf')
 AND c.contype IN ('u','x','f','c') ORDER BY t.relname,c.contype,c.conname;
SELECT 'balanced-multi', c.conname, octet_length(c.conname::text), i.relname = c.conname
 FROM pg_constraint c JOIN pg_class i ON i.oid = c.conindid
 WHERE c.conrelid = 'generated_key_names.mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm'::regclass AND c.contype = 'u';
SELECT 'unicode-multi', c.conname, octet_length(c.conname::text), i.relname = c.conname
 FROM pg_constraint c JOIN pg_class i ON i.oid = c.conindid
 WHERE c.conrelid = 'generated_key_names."éééééééééééééééééééééééééééééé"'::regclass AND c.contype = 'u';
ALTER TABLE generated_key_names.multi ADD CONSTRAINT explicit_key UNIQUE(x,y) DEFERRABLE INITIALLY DEFERRED;
ALTER TABLE generated_key_names.multi RENAME COLUMN x TO retained_x;
REINDEX TABLE CONCURRENTLY generated_key_names.multi;
REINDEX TABLE CONCURRENTLY generated_key_names.mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm;
REINDEX TABLE CONCURRENTLY generated_key_names."éééééééééééééééééééééééééééééé";
SELECT c.conname, i.relname = c.conname FROM pg_constraint c JOIN pg_class i ON i.oid = c.conindid
 WHERE c.conrelid = 'generated_key_names.multi'::regclass AND c.contype = 'u' ORDER BY c.conname;
DROP SCHEMA generated_key_names CASCADE;

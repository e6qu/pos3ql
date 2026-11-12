-- SQLAlchemy's foreign-key reflection joins pg_description through a
-- LEFT-joined pg_constraint and a constant regclass cast.
CREATE SCHEMA cjp;
CREATE TABLE cjp.parent (id int PRIMARY KEY);
CREATE TABLE cjp.child (id int PRIMARY KEY, parent_id int CONSTRAINT child_parent_fk REFERENCES cjp.parent(id));
CREATE TABLE cjp.other (id int, child_id int CONSTRAINT other_child_fk REFERENCES cjp.child(id));
COMMENT ON CONSTRAINT child_parent_fk ON cjp.child IS 'child to parent';
SELECT pg_catalog.pg_class.relname, pg_catalog.pg_constraint.conname, nsp_ref.nspname, cls_ref.relname, pg_catalog.pg_description.description FROM pg_catalog.pg_class LEFT OUTER JOIN pg_catalog.pg_constraint ON pg_catalog.pg_class.oid = pg_catalog.pg_constraint.conrelid AND pg_catalog.pg_constraint.contype = 'f' LEFT OUTER JOIN pg_catalog.pg_class AS cls_ref ON cls_ref.oid = pg_catalog.pg_constraint.confrelid LEFT OUTER JOIN pg_catalog.pg_namespace AS nsp_ref ON cls_ref.relnamespace = nsp_ref.oid LEFT OUTER JOIN pg_catalog.pg_description ON pg_catalog.pg_description.objoid = pg_catalog.pg_constraint.oid AND pg_catalog.pg_description.classoid = CAST('pg_catalog.pg_constraint' AS REGCLASS) JOIN pg_catalog.pg_namespace ON pg_catalog.pg_namespace.oid = pg_catalog.pg_class.relnamespace WHERE pg_catalog.pg_class.relkind = ANY (ARRAY['r', 'p', 'f']) AND pg_catalog.pg_namespace.nspname = 'cjp' ORDER BY 1, 2;
DROP SCHEMA cjp CASCADE;

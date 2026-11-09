CREATE TABLE whole_dep_source(id integer,label text);
INSERT INTO whole_dep_source VALUES(7,'seven');
CREATE VIEW whole_dep_all AS SELECT (t).* FROM whole_dep_source t;
CREATE VIEW whole_dep_nested AS SELECT (v).* FROM whole_dep_all v;
CREATE VIEW whole_dep_qualified AS
SELECT public.whole_dep_all.id FROM public.whole_dep_all;
CREATE VIEW whole_dep_derived AS
SELECT (q).* FROM (SELECT id,label FROM whole_dep_source) q;
CREATE TABLE whole_dep_precision(id integer,spare text);
INSERT INTO whole_dep_precision VALUES(7,'unused');
CREATE VIEW whole_dep_field AS SELECT (t).id AS id FROM whole_dep_precision t;
CREATE VIEW whole_dep_schema_field AS
SELECT public.whole_dep_precision.id FROM public.whole_dep_precision;
ALTER TABLE whole_dep_precision DROP COLUMN spare;
CREATE TYPE whole_dep_pair AS(a integer,b text);
CREATE TABLE whole_dep_record(t whole_dep_pair,spare text);
INSERT INTO whole_dep_record VALUES(ROW(7,'seven')::whole_dep_pair,'unused');
CREATE VIEW whole_dep_record_field AS SELECT (t).a AS id FROM whole_dep_record t;
CREATE VIEW whole_dep_record_star AS SELECT (t).* FROM whole_dep_record t;
ALTER TABLE whole_dep_record DROP COLUMN spare;
CREATE TABLE whole_dep_scalar(t integer,id integer);
CREATE TABLE whole_dep_constructor_source(id integer,spare text);
INSERT INTO whole_dep_constructor_source VALUES(7,'needed by expansion');
CREATE VIEW whole_dep_constructor AS
SELECT (ROW((t).*)).f1 AS id FROM whole_dep_constructor_source t;
CREATE VIEW whole_dep_constructor_prefix AS
SELECT (ROW(-1,(t).*,99)).f2 AS id,(ROW(-1,(t).*,99)).f4 AS tail
FROM whole_dep_constructor_source t;
DO $$BEGIN
    BEGIN
        ALTER TABLE whole_dep_constructor_source DROP COLUMN spare;
        RAISE EXCEPTION 'constructor expansion dependency was lost';
    EXCEPTION WHEN SQLSTATE '2BP01' THEN NULL;
    END;
    BEGIN
        ALTER TABLE whole_dep_source DROP COLUMN label;
        RAISE EXCEPTION 'whole-row dependency was lost';
    EXCEPTION WHEN SQLSTATE '2BP01' THEN NULL;
    END;
    BEGIN
        ALTER TABLE whole_dep_precision DROP COLUMN id;
        RAISE EXCEPTION 'selected-field dependency was lost';
    EXCEPTION WHEN SQLSTATE '2BP01' THEN NULL;
    END;
    BEGIN
        EXECUTE 'CREATE VIEW whole_dep_bad_alias AS SELECT public.whole_dep_all.id FROM whole_dep_all v';
        RAISE EXCEPTION 'hidden source name was accepted';
    EXCEPTION WHEN SQLSTATE '42P01' THEN NULL;
    END;
    BEGIN
        EXECUTE 'CREATE VIEW whole_dep_bad_scalar AS SELECT (t).* FROM whole_dep_scalar t';
        RAISE EXCEPTION 'scalar column was treated as a whole row';
    EXCEPTION WHEN SQLSTATE '42809' THEN NULL;
    END;
END$$;
SELECT n.id,n.label,(SELECT id FROM whole_dep_qualified),
    (SELECT id FROM whole_dep_derived),(SELECT id FROM whole_dep_field),
    (SELECT id FROM whole_dep_schema_field),(SELECT id FROM whole_dep_record_field),
    (SELECT b FROM whole_dep_record_star),(SELECT id FROM whole_dep_constructor),
    (SELECT id FROM whole_dep_constructor_prefix),(SELECT tail FROM whole_dep_constructor_prefix)
FROM whole_dep_nested n;

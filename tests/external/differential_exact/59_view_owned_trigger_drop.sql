CREATE TABLE owned_view_base(id integer);
CREATE VIEW owned_view_parent AS SELECT id FROM owned_view_base;
CREATE FUNCTION owned_view_write() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN INSERT INTO owned_view_base VALUES(NEW.id); RETURN NEW; END$$;
CREATE TRIGGER owned_view_trigger INSTEAD OF INSERT ON owned_view_parent
FOR EACH ROW EXECUTE FUNCTION owned_view_write();
CREATE VIEW owned_view_child AS SELECT id FROM owned_view_parent;

-- next request
DROP VIEW owned_view_parent;

-- next request
SELECT count(*) FROM pg_trigger WHERE tgname='owned_view_trigger';
SELECT count(*) FROM pg_class WHERE relname IN ('owned_view_parent','owned_view_child');
INSERT INTO owned_view_parent VALUES(5) RETURNING id;
SELECT id FROM owned_view_base ORDER BY id;

-- next request
DROP VIEW owned_view_child;
BEGIN;
SAVEPOINT before_drop;
DROP VIEW owned_view_parent;
SELECT count(*) FROM pg_trigger WHERE tgname='owned_view_trigger';
ROLLBACK TO before_drop;
SELECT count(*) FROM pg_trigger WHERE tgname='owned_view_trigger';
INSERT INTO owned_view_parent VALUES(7) RETURNING id;
COMMIT;
SELECT id FROM owned_view_base ORDER BY id;

-- next request
DROP VIEW owned_view_parent;
SELECT count(*) FROM pg_trigger WHERE tgname='owned_view_trigger';
DROP FUNCTION owned_view_write();
DROP TABLE owned_view_base;

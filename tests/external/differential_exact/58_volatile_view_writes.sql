-- View writes evaluate each volatile source, value and predicate once.
CREATE TABLE shared_view_base(id integer PRIMARY KEY, amount integer);
CREATE SEQUENCE shared_view_calls;
CREATE VIEW shared_view AS SELECT id, amount FROM shared_view_base;
CREATE FUNCTION shared_view_write() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO shared_view_base VALUES(NEW.id, NEW.amount);
RETURN NEW;
END IF;
IF TG_OP = 'UPDATE' THEN UPDATE shared_view_base SET amount = NEW.amount WHERE id = OLD.id;
RETURN NEW;
END IF;
DELETE FROM shared_view_base WHERE id = OLD.id;
RETURN OLD;
END $$;
CREATE TRIGGER shared_view_write INSTEAD OF INSERT OR UPDATE OR DELETE ON shared_view FOR EACH ROW EXECUTE FUNCTION shared_view_write();

INSERT INTO shared_view SELECT nextval('shared_view_calls')::integer, 10 RETURNING id, amount;
UPDATE shared_view SET amount = amount + nextval('shared_view_calls')::integer WHERE id = 1 RETURNING id, amount;
DELETE FROM shared_view WHERE id = nextval('shared_view_calls') - 2 RETURNING id;
SELECT currval('shared_view_calls'), count(*) FROM shared_view_base;

INSERT INTO shared_view VALUES(nextval('shared_view_calls')::integer,40) RETURNING id,amount;
UPDATE shared_view SET amount=amount+1 WHERE nextval('shared_view_calls')=5 RETURNING id,amount;
CREATE VIEW shared_volatile_view AS SELECT id,amount+nextval('shared_view_calls')::integer AS amount FROM shared_view_base;
CREATE TRIGGER shared_volatile_write INSTEAD OF UPDATE OR DELETE ON shared_volatile_view FOR EACH ROW EXECUTE FUNCTION shared_view_write();
UPDATE shared_volatile_view SET amount=amount+1 WHERE id=4 RETURNING amount;
SELECT currval('shared_view_calls'),amount FROM shared_view_base;

DELETE FROM shared_view_base;
INSERT INTO shared_view_base VALUES(1,10),(2,20);
ALTER SEQUENCE shared_view_calls RESTART WITH 1;
UPDATE shared_volatile_view AS chosen SET amount=amount+1 WHERE chosen.id=1 RETURNING id,amount;
SELECT currval('shared_view_calls'),amount FROM shared_view_base WHERE id=1;

DELETE FROM shared_view_base;
INSERT INTO shared_view_base VALUES(1,10),(2,20);
ALTER SEQUENCE shared_view_calls RESTART WITH 1;
DELETE FROM shared_volatile_view WHERE public.shared_volatile_view.id=1 RETURNING id,amount;
SELECT currval('shared_view_calls'),count(*) FROM shared_view_base;

DELETE FROM shared_view_base;
INSERT INTO shared_view_base VALUES(1,10),(2,20);
ALTER SEQUENCE shared_view_calls RESTART WITH 1;
UPDATE shared_volatile_view SET amount=amount+1 WHERE id=1 AND amount>0 RETURNING id,amount;
SELECT currval('shared_view_calls');

DELETE FROM shared_view_base;
INSERT INTO shared_view_base VALUES(1,10),(2,20);
ALTER SEQUENCE shared_view_calls RESTART WITH 1;
UPDATE shared_volatile_view SET amount=amount+1 WHERE nextval('shared_view_calls')>0 RETURNING id,amount;
SELECT currval('shared_view_calls');

CREATE SEQUENCE shared_view_filters;
DELETE FROM shared_view_base;
INSERT INTO shared_view_base VALUES(1,10),(2,20);
ALTER SEQUENCE shared_view_calls RESTART WITH 1;
UPDATE shared_volatile_view SET amount=amount+1 WHERE nextval('shared_view_filters')=1 RETURNING id,amount;
SELECT currval('shared_view_calls'),currval('shared_view_filters');
DROP SEQUENCE shared_view_filters;

CREATE VIEW shared_limited_view AS SELECT id,amount+nextval('shared_view_calls')::integer AS amount FROM shared_view_base ORDER BY id LIMIT 1;
CREATE TRIGGER shared_limited_write INSTEAD OF DELETE ON shared_limited_view FOR EACH ROW EXECUTE FUNCTION shared_view_write();
ALTER SEQUENCE shared_view_calls RESTART WITH 1;
DELETE FROM shared_limited_view WHERE id=2 RETURNING id;
SELECT currval('shared_view_calls'),count(*) FROM shared_view_base;
DROP VIEW shared_limited_view;

CREATE VIEW shared_wrapped_view AS SELECT * FROM shared_volatile_view;
CREATE VIEW shared_union_view AS SELECT id,amount+nextval('shared_view_calls')::integer AS amount FROM shared_view_base WHERE id=1
UNION ALL SELECT id,amount+nextval('shared_view_calls')::integer AS amount FROM shared_view_base WHERE id=2;
CREATE VIEW shared_barrier_view WITH (security_barrier=true) AS SELECT id,amount+nextval('shared_view_calls')::integer AS amount FROM shared_view_base;
CREATE TRIGGER shared_wrapped_write INSTEAD OF UPDATE OR DELETE ON shared_wrapped_view FOR EACH ROW EXECUTE FUNCTION shared_view_write();
CREATE TRIGGER shared_union_write INSTEAD OF UPDATE OR DELETE ON shared_union_view FOR EACH ROW EXECUTE FUNCTION shared_view_write();
CREATE TRIGGER shared_barrier_write INSTEAD OF UPDATE OR DELETE ON shared_barrier_view FOR EACH ROW EXECUTE FUNCTION shared_view_write();

DELETE FROM shared_view_base;
INSERT INTO shared_view_base VALUES(1,10),(2,20);
ALTER SEQUENCE shared_view_calls RESTART WITH 1;
UPDATE shared_wrapped_view SET amount=amount+1 WHERE id=1 RETURNING id,amount;
SELECT currval('shared_view_calls');
DELETE FROM shared_view_base;
INSERT INTO shared_view_base VALUES(1,10),(2,20);
ALTER SEQUENCE shared_view_calls RESTART WITH 1;
DELETE FROM shared_wrapped_view WHERE id=1 RETURNING id,amount;
SELECT currval('shared_view_calls'),count(*) FROM shared_view_base;

DELETE FROM shared_view_base;
INSERT INTO shared_view_base VALUES(1,10),(2,20);
ALTER SEQUENCE shared_view_calls RESTART WITH 1;
UPDATE shared_union_view SET amount=amount+1 WHERE id=1 RETURNING id,amount;
SELECT currval('shared_view_calls');
DELETE FROM shared_view_base;
INSERT INTO shared_view_base VALUES(1,10),(2,20);
ALTER SEQUENCE shared_view_calls RESTART WITH 1;
DELETE FROM shared_union_view WHERE id=1 RETURNING id,amount;
SELECT currval('shared_view_calls'),count(*) FROM shared_view_base;

DELETE FROM shared_view_base;
INSERT INTO shared_view_base VALUES(1,10),(2,20);
ALTER SEQUENCE shared_view_calls RESTART WITH 1;
UPDATE shared_barrier_view SET amount=amount+1 WHERE id=1 RETURNING id,amount;
SELECT currval('shared_view_calls');
DELETE FROM shared_view_base;
INSERT INTO shared_view_base VALUES(1,10),(2,20);
ALTER SEQUENCE shared_view_calls RESTART WITH 1;
DELETE FROM shared_barrier_view WHERE id=1 RETURNING id,amount;
SELECT currval('shared_view_calls'),count(*) FROM shared_view_base;

CREATE FUNCTION shared_view_hidden(integer) RETURNS boolean IMMUTABLE LANGUAGE sql SET search_path TO pg_catalog AS $$ SELECT false $$;
DELETE FROM shared_view_base;
INSERT INTO shared_view_base VALUES(1,10),(2,20);
ALTER SEQUENCE shared_view_calls RESTART WITH 1;
UPDATE shared_barrier_view SET amount=amount+1 WHERE shared_view_hidden(id) RETURNING id;
SELECT nextval('shared_view_calls');
ALTER SEQUENCE shared_view_calls RESTART WITH 1;
UPDATE shared_volatile_view SET amount=amount+1 WHERE shared_view_hidden(id) RETURNING id;
SELECT nextval('shared_view_calls');

CREATE FUNCTION shared_view_default(value integer DEFAULT nextval('public.shared_view_calls')::integer) RETURNS integer IMMUTABLE LANGUAGE sql SET search_path TO pg_catalog AS $$ SELECT value $$;
CREATE VIEW shared_default_view AS SELECT id,amount,shared_view_default() AS observed FROM shared_view_base;
CREATE TRIGGER shared_default_write INSTEAD OF UPDATE ON shared_default_view FOR EACH ROW EXECUTE FUNCTION shared_view_write();
DELETE FROM shared_view_base;
INSERT INTO shared_view_base VALUES(1,10),(2,20);
ALTER SEQUENCE shared_view_calls RESTART WITH 1;
UPDATE shared_default_view SET amount=amount+1 WHERE observed=1 RETURNING id,amount,observed;
SELECT currval('shared_view_calls');
DROP VIEW shared_default_view;
DROP FUNCTION shared_view_default(integer);

CREATE FUNCTION shared_view_empty(VARIADIC members integer[]) RETURNS integer IMMUTABLE LANGUAGE sql SET search_path TO pg_catalog AS $$ SELECT cardinality(members) $$;
CREATE VIEW shared_empty_view AS SELECT id,amount+nextval('shared_view_calls')::integer AS amount,shared_view_empty(VARIADIC ARRAY[]::integer[]) AS observed FROM shared_view_base;
CREATE TRIGGER shared_empty_write INSTEAD OF UPDATE ON shared_empty_view FOR EACH ROW EXECUTE FUNCTION shared_view_write();
DELETE FROM shared_view_base;
INSERT INTO shared_view_base VALUES(1,10),(2,20);
ALTER SEQUENCE shared_view_calls RESTART WITH 1;
UPDATE shared_empty_view SET amount=amount+1 WHERE id=1 AND observed=0 RETURNING id,amount,observed;
SELECT currval('shared_view_calls');
DROP VIEW shared_empty_view;
DROP FUNCTION shared_view_empty(integer[]);

DROP VIEW shared_wrapped_view;
DROP VIEW shared_union_view;
DROP VIEW shared_barrier_view;
DROP FUNCTION shared_view_hidden(integer);

DROP VIEW shared_volatile_view;
CREATE VIEW shared_text_view AS SELECT id,amount+nextval('shared_view_calls')::integer AS amount,id::text AS rendered FROM shared_view_base;
CREATE TRIGGER shared_text_write INSTEAD OF UPDATE ON shared_text_view FOR EACH ROW EXECUTE FUNCTION shared_view_write();
DELETE FROM shared_view_base;
INSERT INTO shared_view_base VALUES(1,10),(2,20);
ALTER SEQUENCE shared_view_calls RESTART WITH 1;
UPDATE shared_text_view SET amount=amount+1 WHERE rendered='1' RETURNING id,amount;
SELECT currval('shared_view_calls');
DROP VIEW shared_text_view;
DROP VIEW shared_view;
DROP FUNCTION shared_view_write();
DROP TABLE shared_view_base;
DROP SEQUENCE shared_view_calls;

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
CREATE TRIGGER shared_volatile_write INSTEAD OF UPDATE ON shared_volatile_view FOR EACH ROW EXECUTE FUNCTION shared_view_write();
UPDATE shared_volatile_view SET amount=amount+1 WHERE id=4 RETURNING amount;
SELECT currval('shared_view_calls'),amount FROM shared_view_base;

DROP VIEW shared_volatile_view;
DROP VIEW shared_view;
DROP FUNCTION shared_view_write();
DROP TABLE shared_view_base;
DROP SEQUENCE shared_view_calls;

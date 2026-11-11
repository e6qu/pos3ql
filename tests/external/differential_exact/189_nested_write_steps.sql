-- A rule action whose target has rules of its own runs as a statement inside
-- the outer statement's step. Routine calls in its rows see the rows changed
-- before them, at top level, in a function and in a DO block, and the steps
-- after it still run.
CREATE SCHEMA nested_steps;
SET search_path = nested_steps, public;
CREATE TABLE t(id int, c bigint);
CREATE TABLE log(m text);
CREATE FUNCTION cnt(tag text) RETURNS bigint LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO log VALUES (tag);
  RETURN (SELECT count(*) FROM t) + (SELECT count(*) FROM log);
END$$;
CREATE TABLE u(id int, c bigint DEFAULT cnt('default'));
CREATE TABLE a(n int);
CREATE TABLE b(n int);
CREATE FUNCTION u_after() RETURNS trigger LANGUAGE plpgsql
  AS $$BEGIN INSERT INTO t VALUES (NEW.id, NEW.c); RETURN NEW; END$$;
CREATE TRIGGER u_after AFTER INSERT ON u FOR EACH ROW EXECUTE FUNCTION u_after();
CREATE RULE b_copy AS ON INSERT TO b DO ALSO
  INSERT INTO u VALUES (NEW.n * 10, cnt('rule ' || NEW.n));
CREATE RULE a_copy AS ON INSERT TO a DO ALSO INSERT INTO b VALUES (NEW.n);
CREATE RULE a_mark AS ON INSERT TO a DO ALSO INSERT INTO u(id) VALUES (NEW.n + 100);
CREATE RULE a_last AS ON INSERT TO a DO ALSO INSERT INTO t VALUES (NEW.n + 200, 7);
INSERT INTO a VALUES (1), (2);
CREATE FUNCTION writer(n int) RETURNS int LANGUAGE plpgsql
  AS $$BEGIN INSERT INTO a VALUES (n), (n + 1); RETURN n; END$$;
SELECT writer(3);
DO $$BEGIN INSERT INTO a VALUES (5), (6); END$$;
SELECT * FROM a ORDER BY n;
SELECT * FROM b ORDER BY n;
SELECT * FROM u ORDER BY id;
SELECT * FROM t ORDER BY id;
SELECT m, count(*) FROM log GROUP BY m ORDER BY m;

-- A routine call in a row of a write inside a function runs before the
-- AFTER triggers of the rows changed before it, which fire at the end of
-- the write as in PostgreSQL.
CREATE TABLE f(id int, c bigint DEFAULT cnt('f'));
CREATE TRIGGER f_after AFTER INSERT ON f FOR EACH ROW EXECUTE FUNCTION u_after();
CREATE FUNCTION f_writer() RETURNS int LANGUAGE plpgsql
  AS $$BEGIN INSERT INTO f(id) VALUES (301), (302); RETURN 1; END$$;
SELECT f_writer();
DO $$BEGIN INSERT INTO f(id) VALUES (303), (304); END$$;
SELECT * FROM f ORDER BY id;

-- A rule action stopping with an error undoes the outer statement.
CREATE FUNCTION fails(n int) RETURNS bigint LANGUAGE plpgsql
  AS $$BEGIN IF n = 2 THEN RAISE EXCEPTION 'stop at %', n; END IF; RETURN n; END$$;
CREATE TABLE c(n int);
CREATE TABLE d(n int);
CREATE TABLE e(n int, v bigint);
CREATE RULE d_copy AS ON INSERT TO d DO ALSO INSERT INTO e VALUES (NEW.n, fails(NEW.n));
CREATE RULE c_copy AS ON INSERT TO c DO ALSO INSERT INTO d VALUES (NEW.n);
INSERT INTO c VALUES (1), (2);
SELECT (SELECT count(*) FROM c), (SELECT count(*) FROM d), (SELECT count(*) FROM e);

-- RETURNING in a rule must come from an unconditional INSTEAD rule.
CREATE RULE r1 AS ON INSERT TO c WHERE NEW.n > 0 DO ALSO INSERT INTO e VALUES (NEW.n) RETURNING *;
CREATE RULE r2 AS ON INSERT TO c WHERE NEW.n > 0 DO INSTEAD INSERT INTO e VALUES (NEW.n) RETURNING *;
CREATE RULE r3 AS ON INSERT TO c DO ALSO INSERT INTO e VALUES (NEW.n) RETURNING *;
CREATE RULE r4 AS ON INSERT TO c DO INSTEAD
  (INSERT INTO e VALUES (NEW.n) RETURNING e.n; INSERT INTO e VALUES (NEW.n) RETURNING e.n);

-- A rule may not read a row its event lacks.
CREATE RULE no_old AS ON INSERT TO c DO ALSO INSERT INTO d VALUES (OLD.n);
CREATE RULE no_new AS ON DELETE TO c DO ALSO INSERT INTO d VALUES (NEW.n);
CREATE RULE no_old_condition AS ON INSERT TO c WHERE OLD.n > 0 DO NOTHING;

-- An INSERT's VALUES rows read the routine's variables, never the target's
-- columns, also when rules rewrite the INSERT.
CREATE FUNCTION values_writer(n int) RETURNS int LANGUAGE plpgsql
  AS $$BEGIN INSERT INTO b VALUES (n), (n + 1); RETURN n; END$$;
SELECT values_writer(40);
SELECT * FROM b WHERE n >= 40 ORDER BY n;
INSERT INTO b VALUES (n);
INSERT INTO b VALUES (b.n);
DROP SCHEMA nested_steps CASCADE;

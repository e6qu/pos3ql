-- A rule action's WITH query cannot read the rule's OLD or NEW row, in a
-- data-modifying WITH query or a plain one; OLD is reported first.
CREATE TABLE t423 (a int, b text);
CREATE TABLE log423 (a int);
CREATE RULE r423_new AS ON INSERT TO t423 DO ALSO WITH x AS (INSERT INTO log423 VALUES (new.a) RETURNING a) SELECT a FROM x;
CREATE RULE r423_old AS ON UPDATE TO t423 DO ALSO WITH x AS (DELETE FROM log423 WHERE log423.a = old.a RETURNING *) INSERT INTO log423 SELECT a FROM x;
CREATE RULE r423_plain AS ON UPDATE TO t423 DO ALSO WITH x AS (SELECT old.a, new.a AS na) SELECT * FROM x;
CREATE RULE r423_nested AS ON UPDATE TO t423 DO ALSO WITH x AS (SELECT a FROM log423 WHERE EXISTS (SELECT 1 WHERE log423.a = new.a)) SELECT * FROM x;
SELECT count(*) FROM pg_rewrite WHERE rulename LIKE 'r423%';

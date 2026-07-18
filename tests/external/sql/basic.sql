-- SQL dialect conformance: types, literals, operators, functions,
-- aggregates, DDL/DML, ordering, quoting. Run through real psql; output
-- compared against expected/basic.out.

-- types & literals
SELECT 1, 2.5, 'text', TRUE, FALSE, NULL;
SELECT 1::bigint, '42'::int, 2.7::int, 'on'::bool, true::text, '2.5'::float8;

-- arithmetic, precedence, int8 widening via cast (int4 overflow errors)
SELECT 1 + 2 * 3, (1 + 2) * 3, 7 / 2, 7 % 2, -5, 2147483647::bigint + 1;

-- comparisons, concatenation
SELECT 1 < 2, 2 <= 2, 3 <> 4, 'a' || 'b' || 'c', 'n=' || 42;

-- three-valued logic
SELECT NULL AND FALSE, NULL AND TRUE, NULL OR TRUE, 1 = NULL, NULL IS NULL, 5 IS NOT NULL;

-- scalar functions
SELECT length('héllo'), upper('mIx'), lower('MiX'), abs(-7), coalesce(NULL, 'x'), pg_typeof(1), pg_typeof(1.5);

-- DDL + DML
CREATE TABLE t_basic (id int NOT NULL, name text, score float8, ok bool);
INSERT INTO t_basic VALUES (1, 'ada', 9.5, true), (2, 'bob', 7.25, false), (3, 'cyd', NULL, NULL);
INSERT INTO t_basic (id, name) VALUES (4, 'dee');
SELECT * FROM t_basic ORDER BY id;
SELECT name, score * 2 AS double_score FROM t_basic WHERE score > 7 ORDER BY score DESC;
SELECT name FROM t_basic WHERE ok ORDER BY name;
UPDATE t_basic SET score = score + 0.5 WHERE id = 2;
SELECT score FROM t_basic WHERE id = 2;
DELETE FROM t_basic WHERE id = 4;
SELECT count(*), count(score), sum(id), avg(score), min(name), max(score) FROM t_basic;
SELECT count(*) FROM t_basic WHERE score IS NULL;

-- NULL ordering (PostgreSQL: NULLS LAST asc, NULLS FIRST desc)
CREATE TABLE t_nulls (v int);
INSERT INTO t_nulls VALUES (3), (NULL), (1), (2);
SELECT v FROM t_nulls ORDER BY v;
SELECT v FROM t_nulls ORDER BY v DESC LIMIT 2;

-- identifier quoting, case folding, string syntaxes
CREATE TABLE "MixedCase" ("Id" int, "the name" text);
INSERT INTO "MixedCase" VALUES (1, 'it''s');
SELECT "Id", "the name" FROM "MixedCase";
SELECT $$raw $ text$$, $q$has $$ inside$q$, E'tab\there';

-- multi-word and aliased type names, text coercion on insert
CREATE TABLE t_types (a smallint, b bigint, c double precision, d varchar(10), e boolean);
INSERT INTO t_types VALUES ('5', 9223372036854775807, 1e10, 'str', 'yes');
SELECT * FROM t_types;

-- session statements and transactions (accepted forms)
SET client_min_messages = warning;
SHOW server_version;
SHOW transaction_isolation;
BEGIN;
COMMIT;
ROLLBACK;

-- cleanup
DROP TABLE t_basic;
DROP TABLE t_nulls;
DROP TABLE "MixedCase";
DROP TABLE t_types;
DROP TABLE IF EXISTS never_existed;

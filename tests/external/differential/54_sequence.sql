-- CREATE / ALTER / DROP SEQUENCE and nextval/currval/lastval/setval, matching
-- PostgreSQL 18. A sequence is a first-class relation (relkind 'S'); its value
-- state is non-transactional (advances survive ROLLBACK) and durable.

-- Defaults: ascending from 1, step 1, to bigint max.
CREATE SEQUENCE s1;
SELECT nextval('s1'), nextval('s1'), nextval('s1');
SELECT currval('s1');
SELECT lastval();
SELECT start_value, min_value, max_value, increment_by, cycle, cache_size
  FROM pg_sequences WHERE sequencename = 's1';

-- Set-operation materialization evaluates each volatile branch once.
CREATE SEQUENCE set_operation_sequence;
SELECT nextval('set_operation_sequence') UNION ALL SELECT nextval('set_operation_sequence') ORDER BY 1;
SELECT nextval('set_operation_sequence');

-- setval positions the generator; a two-arg setval sets is_called=true, so the
-- next value steps past it; the three-arg false form makes the next value equal.
SELECT setval('s1', 100);
SELECT nextval('s1');
SELECT setval('s1', 200, false);
SELECT nextval('s1');
-- setval also defines currval (but not lastval).
SELECT setval('s1', 50);
SELECT currval('s1');

-- Options: an explicit range with a step, cycling at the top.
CREATE SEQUENCE s2 START WITH 5 INCREMENT BY 10 MINVALUE 5 MAXVALUE 25 CYCLE;
SELECT nextval('s2'), nextval('s2'), nextval('s2');
SELECT nextval('s2');   -- cycles back to MINVALUE

-- A non-cycling sequence runs off its bound with 2200H.
CREATE SEQUENCE s3 MAXVALUE 2 NO CYCLE;
SELECT nextval('s3'), nextval('s3');
SELECT nextval('s3');   -- ERROR 2200H

-- Descending defaults: max -1, min the type floor, start at max.
CREATE SEQUENCE s4 INCREMENT BY -1;
SELECT start_value, min_value, max_value FROM pg_sequences WHERE sequencename = 's4';
SELECT nextval('s4'), nextval('s4');

-- AS <type> sets the default bounds and the reported data type.
CREATE SEQUENCE s5 AS smallint;
SELECT data_type, min_value, max_value FROM pg_sequences WHERE sequencename = 's5';
CREATE SEQUENCE s6 AS integer INCREMENT -1;
SELECT data_type, min_value, max_value, start_value FROM pg_sequences WHERE sequencename = 's6';

-- ALTER SEQUENCE redefines parameters; RESTART repositions the generator.
CREATE SEQUENCE s7 START 1;
SELECT nextval('s7'), nextval('s7');
ALTER SEQUENCE s7 RESTART WITH 50;
SELECT nextval('s7');
ALTER SEQUENCE s7 INCREMENT BY 5 MAXVALUE 1000;
SELECT nextval('s7');
SELECT increment_by, max_value FROM pg_sequences WHERE sequencename = 's7';
ALTER SEQUENCE s7 RESTART;
SELECT nextval('s7');

-- Catalog: relkind 'S'; pg_sequences lists it; last_value is NULL until first use.
SELECT relkind FROM pg_class WHERE relname = 's1';
CREATE SEQUENCE s8 START 7;
SELECT last_value FROM pg_sequences WHERE sequencename = 's8';  -- NULL, never called
SELECT nextval('s8');
SELECT last_value FROM pg_sequences WHERE sequencename = 's8';  -- 7

-- Sequences feed columns explicitly via nextval in INSERT (advances per row).
CREATE TABLE items (id bigint, label text);
CREATE SEQUENCE items_seq;
INSERT INTO items VALUES (nextval('items_seq'), 'a'), (nextval('items_seq'), 'b');
INSERT INTO items SELECT nextval('items_seq'), 'c';
SELECT id, label FROM items ORDER BY id;
-- UPDATE ... SET = nextval advances once per updated row. (Which row receives
-- which value depends on the update's visitation order, which SQL leaves
-- unspecified, so compare the order-independent set of assigned ids.)
UPDATE items SET id = nextval('items_seq');
SELECT id FROM items ORDER BY id;

-- Advances are non-transactional: they survive ROLLBACK (PostgreSQL's gaps).
CREATE SEQUENCE s9;
BEGIN;
SELECT nextval('s9');
SELECT nextval('s9');
ROLLBACK;
SELECT nextval('s9');   -- continues from 2, not 1

-- Errors: currval before nextval (55000); unknown sequence (42P01); wrong type
-- (42809); bad parameters (22023); duplicate (42P07).
CREATE SEQUENCE s10;
SELECT currval('s10');
SELECT nextval('nope');
CREATE TABLE t (a int);
SELECT nextval('t');
DROP SEQUENCE t;
DROP TABLE s1;
CREATE SEQUENCE bad MINVALUE 10 MAXVALUE 5;
CREATE SEQUENCE bad2 START 100 MINVALUE 1 MAXVALUE 50;
CREATE SEQUENCE bad3 INCREMENT 0;
CREATE SEQUENCE bad4 AS text;
CREATE SEQUENCE bad5 AS smallint MAXVALUE 40000;
CREATE SEQUENCE s1;

-- IF NOT EXISTS / IF EXISTS.
CREATE SEQUENCE IF NOT EXISTS s1;
DROP SEQUENCE IF EXISTS nope;
DROP SEQUENCE nope;

-- DROP removes it.
DROP SEQUENCE s1;
SELECT nextval('s1');

-- Clean up leftover tables so later corpora (shared database) do not collide.
DROP TABLE IF EXISTS items;
DROP TABLE IF EXISTS t;

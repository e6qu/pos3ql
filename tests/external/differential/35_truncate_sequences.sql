-- TRUNCATE and true sequence semantics for serial columns.
-- Each probe diverged before the fix (auto-increment was max-based).
CREATE TABLE ts (id serial, v int);

-- An explicit value does not advance the sequence.
INSERT INTO ts(id, v) VALUES (10, 1);
INSERT INTO ts(v) VALUES (2);
SELECT id, v FROM ts ORDER BY v;

-- Deleting rows does not rewind it.
DELETE FROM ts;
INSERT INTO ts(v) VALUES (3);
SELECT id, v FROM ts;

-- A rolled-back insert still consumes its number.
BEGIN; INSERT INTO ts(v) VALUES (4); ROLLBACK;
INSERT INTO ts(v) VALUES (5);
SELECT id, v FROM ts ORDER BY v;

-- TRUNCATE continues the sequence; RESTART IDENTITY resets it.
TRUNCATE ts;
INSERT INTO ts(v) VALUES (6);
SELECT id, v FROM ts;
TRUNCATE ts RESTART IDENTITY;
INSERT INTO ts(v) VALUES (7);
SELECT id, v FROM ts;

-- Both the row removal and the reset are transactional.
BEGIN; TRUNCATE ts RESTART IDENTITY; ROLLBACK;
INSERT INTO ts(v) VALUES (8);
SELECT id, v FROM ts ORDER BY v;
BEGIN; TRUNCATE ts; ROLLBACK;
SELECT count(*) FROM ts;
BEGIN; TRUNCATE ts RESTART IDENTITY; INSERT INTO ts(v) VALUES (9); SELECT id FROM ts; COMMIT;
SELECT id, v FROM ts;

-- CONTINUE IDENTITY is the spelled-out default.
TRUNCATE TABLE ts CONTINUE IDENTITY;
INSERT INTO ts(v) VALUES (10);
SELECT id FROM ts;

-- An explicit NULL into a serial column is a not-null violation, not a
-- generated value.
INSERT INTO ts(id) VALUES (NULL);
DROP TABLE ts;

-- The structural foreign-key rule and its CASCADE closure.
CREATE TABLE tp (id int PRIMARY KEY);
CREATE TABLE tc (id int REFERENCES tp(id));
TRUNCATE tp;
TRUNCATE tp, tc;
TRUNCATE tc, tp;
TRUNCATE tp CASCADE;
TRUNCATE nosuch;
CREATE VIEW tv AS SELECT 1;
TRUNCATE tv;
DROP VIEW tv;
DROP TABLE tc;
DROP TABLE tp;

-- Multi-table truncate with independent sequences.
CREATE TABLE ta (id serial, v int);
CREATE TABLE tb (id serial, v int);
INSERT INTO ta(v) VALUES (1),(2);
INSERT INTO tb(v) VALUES (1);
TRUNCATE ta, tb;
INSERT INTO ta(v) VALUES (3);
INSERT INTO tb(v) VALUES (3);
SELECT id FROM ta;
SELECT id FROM tb;
TRUNCATE ta, tb RESTART IDENTITY;
INSERT INTO ta(v) VALUES (4);
SELECT id FROM ta;
DROP TABLE ta;
DROP TABLE tb;

-- smallserial bounds ride the sequence.
CREATE TABLE sb (id smallserial, v int);
INSERT INTO sb(id, v) VALUES (32767, 1);
INSERT INTO sb(v) VALUES (2);
SELECT id, v FROM sb ORDER BY v;
DROP TABLE sb;

-- Error conformance: every failure must carry the right SQLSTATE
-- (VERBOSITY verbose makes psql print it) and later statements must
-- keep working.
\set VERBOSITY verbose

SELECT 1/0;
SELECT 1.0/0;
SELECT * FROM missing_table;
CREATE TABLE t_err (id int NOT NULL, v text);
SELECT no_such_column FROM t_err;
INSERT INTO t_err VALUES (NULL, 'x');
INSERT INTO t_err (no_col) VALUES (1);
CREATE TABLE t_err (id int);
SELECT 'zap'::int;
SELECT 1::nosuchtype;
SELECT 9223372036854775807 + 1;
SELECT nosuchfunc(1);
SELECT 1 +;
SELECT count(*), id FROM t_err;
DROP TABLE missing_table;
-- psql sends each ;-separated statement as its own Query message and
-- (with ON_ERROR_STOP off) continues after errors; the wire_probe covers
-- true multi-statement strings in one message.
SELECT 'before'; SELECT 1/0; SELECT 'after';
-- the session stays healthy after every error above
SELECT 'session still works';
DROP TABLE t_err;

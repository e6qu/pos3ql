-- Duplicate keys met while a unique index is built (CREATE INDEX, a new
-- UNIQUE or PRIMARY KEY constraint, REINDEX, a table rewrite) report
-- PostgreSQL's index-build error; a duplicate written row does not.
CREATE SCHEMA unique_builds;
SET search_path = unique_builds, public;
CREATE TABLE u (a int, b int);
INSERT INTO u VALUES (1, 1), (1, 2);
CREATE UNIQUE INDEX u_a ON u (a);
ALTER TABLE u ADD CONSTRAINT u_a_key UNIQUE (a);
ALTER TABLE u ADD PRIMARY KEY (a);
CREATE UNIQUE INDEX u_ab ON u (a, b);
INSERT INTO u VALUES (1, 1);
CREATE INDEX CONCURRENTLY u_c ON u (a);
CREATE UNIQUE INDEX CONCURRENTLY u_d ON u (a);
REINDEX INDEX u_ab;
CREATE TABLE w (a int, b text);
INSERT INTO w VALUES (1, '1'), (2, '01');
CREATE UNIQUE INDEX w_b ON w ((b::int));
CREATE UNIQUE INDEX w_bp ON w (b) WHERE a > 0;
ALTER TABLE w ADD CONSTRAINT w_u UNIQUE (b);
ALTER TABLE w ALTER COLUMN b TYPE int USING b::int;
ALTER TABLE w ADD COLUMN c int DEFAULT 1, ADD CONSTRAINT w_c UNIQUE (c);
RESET search_path;
DROP SCHEMA unique_builds CASCADE;

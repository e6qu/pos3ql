-- A single-column UNIQUE / PRIMARY KEY with an explicit name keeps that name
-- (previously a single-column key rode a nameless column flag): the violation
-- message quotes the given name, DROP CONSTRAINT and RENAME CONSTRAINT find it,
-- DROP NOT NULL on a primary-key column is refused, and renaming an unnamed
-- single-column key by its generated name materializes it. The names are
-- invisible to the SQLSTATE-normalized corpus, so they are pinned here. Matches
-- PostgreSQL 18.
DROP TABLE IF EXISTS nk;
DROP TABLE IF EXISTS np;
DROP TABLE IF EXISTS nu;

-- Column-level UNIQUE with an explicit constraint name.
CREATE TABLE nk (a int CONSTRAINT myc UNIQUE, b int);
INSERT INTO nk VALUES (1, 1);
INSERT INTO nk VALUES (1, 2);
ALTER TABLE nk DROP CONSTRAINT myc;
INSERT INTO nk VALUES (1, 3);

-- Column-level PRIMARY KEY with an explicit name: the violation names it, and
-- DROP NOT NULL on the key column is refused.
CREATE TABLE np (id int CONSTRAINT np_pk PRIMARY KEY, v int);
INSERT INTO np VALUES (1, 1);
INSERT INTO np VALUES (1, 2);
ALTER TABLE np ALTER COLUMN id DROP NOT NULL;

-- An unnamed single-column key is renamed by its generated name.
CREATE TABLE nu (x int UNIQUE);
ALTER TABLE nu RENAME CONSTRAINT nu_x_key TO xkey;
INSERT INTO nu VALUES (5);
INSERT INTO nu VALUES (5);
ALTER TABLE nu DROP CONSTRAINT xkey;
INSERT INTO nu VALUES (5);

DROP TABLE nk;
DROP TABLE np;
DROP TABLE nu;

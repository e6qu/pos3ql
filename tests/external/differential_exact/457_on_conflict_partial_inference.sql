-- ON CONFLICT (cols) WHERE pred infers a partial unique index whose
-- predicate the WHERE implies; only rows inside that index conflict.
CREATE TABLE oc457 (a int, b int, c text);
CREATE UNIQUE INDEX oc457_pos ON oc457 (a) WHERE a > 0;
INSERT INTO oc457 VALUES (1, 1, 'one'), (-1, 1, 'negative');
INSERT INTO oc457 VALUES (1, 2, 'dup') ON CONFLICT (a) WHERE a > 0 DO NOTHING;
INSERT INTO oc457 VALUES (1, 3, 'upd') ON CONFLICT (a) WHERE a > 0 DO UPDATE SET c = excluded.c;
INSERT INTO oc457 VALUES (1, 4, 'x') ON CONFLICT (a) DO NOTHING;
INSERT INTO oc457 VALUES (1, 4, 'implied') ON CONFLICT (a) WHERE a > 5 DO UPDATE SET c = 'implied';
INSERT INTO oc457 VALUES (1, 4, 'x') ON CONFLICT (a) WHERE b > 0 DO NOTHING;
INSERT INTO oc457 VALUES (1, 4, 'x') ON CONFLICT (a) WHERE a > -5 DO NOTHING;
-- A row outside the index never conflicts.
INSERT INTO oc457 VALUES (-1, 5, 'outside') ON CONFLICT (a) WHERE a > 0 DO UPDATE SET c = 'never';
SELECT * FROM oc457 ORDER BY a, b;
-- Two key columns and a conjunctive predicate.
CREATE TABLE oc457_two (a int, b int, live bool, n int DEFAULT 0);
CREATE UNIQUE INDEX oc457_two_live ON oc457_two (b, a) WHERE live AND a > 0;
INSERT INTO oc457_two VALUES (1, 1, true);
INSERT INTO oc457_two VALUES (1, 1, true) ON CONFLICT (a, b) WHERE live AND a > 0
  DO UPDATE SET n = oc457_two.n + 1;
INSERT INTO oc457_two VALUES (1, 1, true) ON CONFLICT (a, b) WHERE live DO NOTHING;
SELECT * FROM oc457_two;
-- A full unique index needs no predicate, so a WHERE still infers it.
CREATE TABLE oc457_full (a int UNIQUE, b int);
INSERT INTO oc457_full VALUES (1, 1);
INSERT INTO oc457_full VALUES (1, 2) ON CONFLICT (a) WHERE a > 0 DO UPDATE SET b = excluded.b;
SELECT * FROM oc457_full;

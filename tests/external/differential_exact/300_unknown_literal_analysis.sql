-- Unknown-type literals convert while a statement is analysed, as their
-- operator, common type or assignment target resolves them: errors arrive
-- before any row is read, at PREPARE and at CREATE RULE, positioned at the
-- literal; coalesce, greatest and least convert them to the common type.
CREATE TABLE t300 (a int, b text, d date);
SELECT now() - 'zz' FROM t300;
SELECT now() - 'zz';
SELECT a FROM t300 WHERE d > 'garbage';
UPDATE t300 SET a = 'k';
INSERT INTO t300 (a) SELECT 'k' FROM t300;
SELECT coalesce(a, 'r') FROM t300;
SELECT greatest(a, 'z') FROM t300;
SELECT CASE WHEN a > 0 THEN a ELSE 'w' END FROM t300;
SELECT a FROM t300 WHERE a BETWEEN '1' AND 'v';
SELECT a::text FROM t300 WHERE d IN ('2020-01-01', 'bad');
SELECT upper(b) FROM t300 WHERE b LIKE 'x' AND d < 'yesterday-ish';
PREPARE p300a AS SELECT a + 'x' FROM t300;
PREPARE p300b AS SELECT 'a'::int FROM t300;
PREPARE p300c AS SELECT now() - 'zz' FROM t300;
PREPARE p300d AS UPDATE t300 SET a = 'k';
PREPARE p300e AS INSERT INTO t300 (a) VALUES ('k');
PREPARE p300f AS INSERT INTO t300 (a) SELECT 'k' FROM t300;
PREPARE p300g AS SELECT lower(b) FROM t300 WHERE b = 'x' AND a = '1' ORDER BY 'x'::int;
PREPARE p300h AS SELECT coalesce(a, 'r') FROM t300;
PREPARE p300i AS DELETE FROM t300 WHERE a = 'k';
CREATE RULE r300 AS ON INSERT TO t300 DO ALSO INSERT INTO t300 (a) VALUES ('zz');
CREATE RULE r300 AS ON INSERT TO t300 DO ALSO SELECT new.a + 'x';
CREATE RULE r300 AS ON INSERT TO t300 DO ALSO UPDATE t300 SET a = 'k';
INSERT INTO t300 VALUES (1, 'b', '2024-02-03'), (NULL, NULL, NULL);
SELECT pg_typeof(coalesce(a, '3')), coalesce(a, '3') + 1, greatest(a, '3'), least(d, '2020-01-01') FROM t300 ORDER BY a;

-- PREPARE types each undeclared parameter from its use, as PostgreSQL's
-- analysis does, and positions analysis errors in the PREPARE text.
CREATE TABLE t283 (label text, n int);
INSERT INTO t283 VALUES ('1', 1), ('3', 3);
PREPARE after_label AS SELECT label FROM t283 WHERE label > $1;
EXECUTE after_label(2);
PREPARE from_call AS SELECT abs($1), sqrt($2), upper($3), length($4);
EXECUTE from_call(-1, 4, 'ab', 'abc');
PREPARE rounded AS SELECT round($1);
EXECUTE rounded(2.5);
PREPARE shifted AS SELECT $1 + timestamptz '2020-01-01 00:00+00', now() - $2 > now() - $2;
PREPARE scaled AS SELECT interval '1 day' * $1, $2 - 1::bigint, $3 + 1.5;
PREPARE compared AS SELECT $1 = n, $2 || label FROM t283 WHERE n IN ($3, $4);
EXECUTE compared(1, 'x', 1, 3);
SELECT name, parameter_types::text, result_types::text
  FROM pg_prepared_statements ORDER BY name;
PREPARE untyped AS SELECT pg_typeof($1);
PREPARE skipped AS SELECT $2::int;
PREPARE wrong_case AS SELECT CASE $1 WHEN 1 THEN 'a' END;
PREPARE wrong_sublink AS SELECT (SELECT 1, 2);
SELECT CASE label WHEN 'x' THEN 1 WHEN 3 THEN 2 END FROM t283;
SELECT CASE 1 WHEN 2 THEN 1
              WHEN 'a'::text THEN 2 END;
EXECUTE rounded(1, 2);

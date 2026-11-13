-- A STABLE or IMMUTABLE SQL function reads the catalog under its calling
-- statement's snapshot, so tables a VOLATILE routine creates earlier in the
-- same statement stay invisible to it; a VOLATILE SQL function sees them.
CREATE FUNCTION mk352(i int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN EXECUTE format('CREATE TABLE made352_%s (a int)', i); RETURN i; END $$;
CREATE FUNCTION count_stable352() RETURNS bigint LANGUAGE sql STABLE
  AS $$ SELECT count(*) FROM pg_class WHERE relname LIKE 'made352%' $$;
CREATE FUNCTION count_immutable352() RETURNS bigint LANGUAGE sql IMMUTABLE
  RETURN (SELECT count(*) FROM pg_class WHERE relname LIKE 'made352%');
CREATE FUNCTION count_volatile352() RETURNS bigint LANGUAGE sql VOLATILE
  AS $$ SELECT count(*) FROM pg_class WHERE relname LIKE 'made352%' $$;
CREATE FUNCTION count_nested352() RETURNS bigint LANGUAGE sql STABLE
  AS $$ SELECT count_stable352() + 0 $$;
SELECT mk352(g), count_stable352(), count_volatile352() FROM generate_series(1, 3) g;
SELECT mk352(g), count_nested352(), count_immutable352() + 0 FROM generate_series(4, 5) g;
SELECT count_stable352()
UNION ALL SELECT mk352(6)
UNION ALL SELECT count_stable352();
WITH made AS (SELECT mk352(7) AS m) SELECT m, count_stable352() FROM made;
SELECT count_stable352(), count_volatile352();
CREATE TABLE log352 (made int, seen bigint);
INSERT INTO log352 VALUES (mk352(8), count_stable352()) RETURNING *;
UPDATE log352 SET seen = count_stable352() + mk352(9) RETURNING *;
SELECT count_stable352();
DROP TABLE made352_1, made352_2, made352_3, made352_4, made352_5, made352_6, made352_7,
  made352_8, made352_9, log352;
DROP FUNCTION mk352(int), count_stable352(), count_immutable352(), count_volatile352(),
  count_nested352();

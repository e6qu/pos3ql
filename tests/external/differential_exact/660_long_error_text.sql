-- Error messages, DETAIL and HINT quote long input whole.
SELECT ('2024-01-05 ' || repeat('9', 300))::date;
SELECT (repeat('2024-01-05 ', 30) || 'x')::timestamptz;
SELECT (repeat('1', 250) || 'x')::numeric;
SELECT ('1.' || repeat('5', 200) || 'e')::float8;
SELECT ('{"' || repeat('k', 300) || '": tru}')::json;
SELECT ('[' || repeat('1, ', 100) || 'nope]')::jsonb;
SELECT ('{' || repeat('1,', 120) || 'x')::int[];
SELECT (repeat('ab', 150) || '!')::uuid;
SELECT (repeat('x', 400))::interval;
SELECT (repeat('z', 260))::boolean;
-- Identifiers near NAMEDATALEN: names longer than 63 bytes are truncated,
-- and a message quotes the truncated name.
CREATE TABLE abcdefghij_abcdefghij_abcdefghij_abcdefghij_abcdefghij_abcdefghij_xyz (a int);
SELECT * FROM abcdefghij_abcdefghij_abcdefghij_abcdefghij_abcdefghij_abcdefghij_xyz_other_suffix;
SELECT * FROM missing_schema_abcdefghij_abcdefghij_abcdefghij_abcdefghij_abcdefghij.missing_table_abcdefghij_abcdefghij_abcdefghij_abcdefghij_abcdefghij;
SELECT abcdefghij_abcdefghij_abcdefghij_abcdefghij_abcdefghij_abcdefghij_xyz.missing_column_abcdefghij_abcdefghij_abcdefghij_abcdefghij_abcdefghij FROM abcdefghij_abcdefghij_abcdefghij_abcdefghij_abcdefghij_abcdefghij_xyz;
DROP TABLE abcdefghij_abcdefghij_abcdefghij_abcdefghij_abcdefghij_abcdefghij_xyz;
-- A procedural handler sees the whole message, and RAISE reports long
-- message, DETAIL and HINT text whole.
DO $$
BEGIN
  PERFORM ('2024-01-05 ' || repeat('8', 300))::date;
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE 'caught % bytes: %', length(SQLERRM), right(SQLERRM, 20);
END $$;
DO $$
DECLARE
  message text;
BEGIN
  PERFORM (repeat('7', 400) || 'x')::numeric;
EXCEPTION WHEN OTHERS THEN
  GET STACKED DIAGNOSTICS message = MESSAGE_TEXT;
  RAISE NOTICE 'stacked % bytes', length(message);
  RAISE;
END $$;
DO $$
BEGIN
  RAISE EXCEPTION 'long message %', repeat('m', 500)
    USING DETAIL = 'long detail ' || repeat('d', 500),
          HINT = 'long hint ' || repeat('h', 500);
END $$;
DO $$
BEGIN
  RAISE NOTICE 'long notice %', repeat('n', 300);
END $$;
-- A long statement in an error's context.
CREATE FUNCTION long_context() RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  RETURN (SELECT ('2024-01-05 ' || repeat('6', 250))::date - date '2024-01-01' + length('padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding'));
END $$;
SELECT long_context();
DROP FUNCTION long_context();

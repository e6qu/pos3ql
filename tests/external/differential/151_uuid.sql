-- PostgreSQL 18 UUID parsing, generation, inspection, catalogs, and durable
-- default-expression behavior.
SET TIME ZONE 'UTC';

SELECT '{a0eebc999c0b4ef8bb6d6bb9bd380a11}'::uuid,
       'a0ee-bc99-9c0b-4ef8-bb6d-6bb9-bd38-0a11'::uuid,
       'a0eebc999c0b4ef8bb6d6bb9bd380a11'::uuid;

SELECT oid, proname, prorettype, proargtypes, proargnames,
       provolatile, proparallel, proisstrict, proleakproof, prosrc
  FROM pg_proc
 WHERE oid IN (3432, 6428, 6429, 6430, 6342, 6343)
 ORDER BY oid;

SELECT uuid_extract_version(gen_random_uuid()),
       uuid_extract_version(uuidv4()),
       uuid_extract_version(uuidv7()),
       uuidv7() < uuidv7(),
       uuid_extract_timestamp(uuidv7()) IS NOT NULL,
       pg_catalog.uuid_extract_version(pg_catalog.uuidv7());
SELECT uuid_extract_timestamp(uuidv7(interval '-1 day')) < clock_timestamp(),
       uuid_extract_timestamp(uuidv7(shift => interval '1 day')) > clock_timestamp();
SET TIME ZONE 'America/New_York';
SELECT '2024-03-09 12:00:00-05'::timestamptz + interval '1 day';
SELECT extract(year FROM timestamp with time zone '2024-01-15 12:34:56-05'),
       extract(month FROM timestamp with time zone '2024-01-15 12:34:56-05'),
       extract(day FROM timestamp with time zone '2024-01-15 12:34:56-05'),
       extract(hour FROM timestamp with time zone '2024-01-15 12:34:56-05'),
       extract(timezone FROM timestamp with time zone '2024-01-15 12:34:56-05'),
       extract(timezone_hour FROM timestamp with time zone '2024-01-15 12:34:56-05'),
       date_part('epoch', timestamp with time zone '2024-01-15 12:34:56-05');
SELECT timestamp '2024-03-10 02:30' AT TIME ZONE 'America/New_York',
       timestamp '2024-11-03 01:30' AT TIME ZONE 'America/New_York',
       date_trunc('year', timestamp with time zone '2024-07-15 12:34-04'),
       make_timestamptz(2024, 7, 15, 12, 0, 0),
       '2024-01-15'::timestamptz;
SELECT extract(hour FROM uuid_extract_timestamp(uuidv7(interval '-8 months'))) =
       extract(hour FROM clock_timestamp());
SET TIME ZONE 'UTC';

SELECT uuid_extract_timestamp('018cc251-f400-7abc-8000-000000000000'),
       uuid_extract_timestamp('04c296c2-0c98-11f0-8000-000000000000'),
       uuid_extract_timestamp('a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11') IS NULL;
SELECT uuid_extract_version('a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'),
       uuid_extract_version('a0eebc99-9c0b-0ef8-bb6d-6bb9bd380a11'),
       uuid_extract_version('a0eebc99-9c0b-4ef8-3b6d-6bb9bd380a11') IS NULL;
SELECT uuid_extract_timestamp(NULL::uuid) IS NULL,
       uuid_extract_version(NULL::uuid) IS NULL,
       uuidv7(NULL::interval) IS NULL;

CREATE TABLE uuid_function_values (
  id uuid PRIMARY KEY DEFAULT uuidv7(),
  random_id uuid UNIQUE NOT NULL DEFAULT gen_random_uuid(),
  created_at timestamptz GENERATED ALWAYS AS (uuid_extract_timestamp(id)) STORED,
  version smallint GENERATED ALWAYS AS (uuid_extract_version(id)) STORED,
  CHECK (uuid_extract_version(id) = 7),
  CHECK (uuid_extract_version(random_id) = 4)
);
INSERT INTO uuid_function_values DEFAULT VALUES;
INSERT INTO uuid_function_values DEFAULT VALUES;
SELECT count(*), count(DISTINCT id), count(DISTINCT random_id),
       min(version), max(version), bool_and(created_at IS NOT NULL)
  FROM uuid_function_values;

PREPARE uuid_inspect(uuid) AS
  SELECT uuid_extract_version($1), uuid_extract_timestamp($1);
EXECUTE uuid_inspect('018cc251-f400-7abc-8000-000000000000');
DEALLOCATE uuid_inspect;

CREATE FUNCTION uuid_function_contract(input uuid) RETURNS TABLE(version smallint, present boolean)
LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT uuid_extract_version(input), uuid_extract_timestamp(input) IS NOT NULL;
END
$$;
SELECT * FROM uuid_function_contract('018cc251-f400-7abc-8000-000000000000');

SELECT ' a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11 '::uuid;
SELECT 'a-0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid;
SELECT uuid_extract_version('a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::text);
SELECT uuidv7(bogus => interval '1 day');

DROP FUNCTION uuid_function_contract(uuid);
DROP TABLE uuid_function_values;

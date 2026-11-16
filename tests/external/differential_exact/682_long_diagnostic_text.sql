-- DETAIL and HINT are kept whole however long they are, like the message:
-- here 20 KB and 15 KB, and a 14 KB key in a unique violation's DETAIL.
DO $$
BEGIN
  RAISE EXCEPTION 'long diagnostic'
    USING DETAIL = 'D' || repeat('d', 20000) || 'E',
          HINT = 'H' || repeat('h', 15000) || 'I';
END $$;
DO $$ BEGIN RAISE NOTICE 'notice' USING DETAIL = 'D' || repeat('d', 20000) || 'E'; END $$;
DO $$
DECLARE
  detail text;
  hint text;
BEGIN
  RAISE EXCEPTION 'caught' USING DETAIL = repeat('x', 13000), HINT = repeat('y', 13000);
EXCEPTION WHEN OTHERS THEN
  GET STACKED DIAGNOSTICS detail = PG_EXCEPTION_DETAIL, hint = PG_EXCEPTION_HINT;
  RAISE NOTICE 'detail % bytes, hint % bytes', length(detail), length(hint);
  RAISE;
END $$;
CREATE TABLE long_keys (t text UNIQUE);
INSERT INTO long_keys VALUES (repeat('ab', 7000));
INSERT INTO long_keys VALUES (repeat('ab', 7000));
DROP TABLE long_keys;

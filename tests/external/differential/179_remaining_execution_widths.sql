-- Mutable routine replay and cursor row indexes are bounded by their startup
-- memory budgets, not the former 1,024-call and 65,536-row envelopes.
DROP FUNCTION IF EXISTS execution_width_record(integer);
DROP TABLE IF EXISTS execution_width_results;
DROP TABLE IF EXISTS execution_width_audit;

CREATE TABLE execution_width_audit(value integer);
CREATE TABLE execution_width_results(value integer);
CREATE FUNCTION execution_width_record(input_value integer) RETURNS integer
LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO execution_width_audit VALUES (input_value);
  RETURN input_value;
END
$$;

INSERT INTO execution_width_results
SELECT execution_width_record(value)
FROM generate_series(1, 1100) AS values(value);
SELECT count(*), min(value), max(value) FROM execution_width_results;
SELECT count(*), min(value), max(value) FROM execution_width_audit;

BEGIN;
DECLARE execution_width_cursor SCROLL CURSOR FOR
  SELECT value FROM generate_series(1, 70000) AS values(value);
FETCH ABSOLUTE 65537 FROM execution_width_cursor;
FETCH LAST FROM execution_width_cursor;
COMMIT;

-- cleanup
DROP FUNCTION execution_width_record(integer);
DROP TABLE execution_width_results;
DROP TABLE execution_width_audit;

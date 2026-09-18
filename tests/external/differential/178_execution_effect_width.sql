-- Retry effects and event-trigger graphs are bounded by statement memory,
-- not the former 1,024-call and 256-object implementation envelopes.
DROP EVENT TRIGGER IF EXISTS effect_width_drop;
DROP FUNCTION IF EXISTS effect_width_capture();
DROP TABLE IF EXISTS effect_width_audit;
DROP SCHEMA IF EXISTS effect_width_graph CASCADE;
DROP SEQUENCE IF EXISTS effect_width_sequence;

CREATE SEQUENCE effect_width_sequence;
SELECT count(*), min(value), max(value)
FROM (
  SELECT nextval('effect_width_sequence') AS value
  FROM generate_series(1, 1100)
) AS calls;

CREATE TABLE effect_width_audit(objects bigint, originals bigint);
CREATE FUNCTION effect_width_capture() RETURNS event_trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO effect_width_audit
  SELECT count(*), sum(CASE WHEN original THEN 1 ELSE 0 END)
  FROM pg_event_trigger_dropped_objects();
  RETURN;
END
$$;
CREATE EVENT TRIGGER effect_width_drop ON sql_drop
  EXECUTE FUNCTION effect_width_capture();
CREATE SCHEMA effect_width_graph;
CREATE TABLE effect_width_graph.t_0(value integer PRIMARY KEY);
DO $$
DECLARE
  slot integer;
BEGIN
  FOR slot IN 1..32 LOOP
    EXECUTE 'CREATE TABLE effect_width_graph.t_' || slot ||
            '(value integer PRIMARY KEY, parent integer REFERENCES effect_width_graph.t_0(value))';
  END LOOP;
END
$$;
DROP SCHEMA effect_width_graph CASCADE;
SELECT objects, originals FROM effect_width_audit;

DROP EVENT TRIGGER effect_width_drop;
DROP FUNCTION effect_width_capture();
DROP TABLE effect_width_audit;
DROP SEQUENCE effect_width_sequence;

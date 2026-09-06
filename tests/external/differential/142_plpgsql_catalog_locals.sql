-- PL/pgSQL declarations retain catalog type identity rather than reducing
-- enum, domain, and composite locals to their runtime base representation.
DROP TABLE IF EXISTS plpgsql_typed_local_rows;
DROP TABLE IF EXISTS plpgsql_typed_local_audit;
DROP DOMAIN IF EXISTS plpgsql_typed_local_positive CASCADE;
DROP TYPE IF EXISTS plpgsql_typed_local_pair CASCADE;
DROP TYPE IF EXISTS plpgsql_typed_local_state CASCADE;

CREATE TYPE plpgsql_typed_local_state AS ENUM ('ready', 'done');
CREATE TYPE plpgsql_typed_local_pair AS (number integer, label text);
CREATE DOMAIN plpgsql_typed_local_positive AS integer CHECK (VALUE > 0);
CREATE TABLE plpgsql_typed_local_audit (label text);

CREATE FUNCTION plpgsql_typed_local_value(input_value plpgsql_typed_local_positive)
RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  checked plpgsql_typed_local_positive := input_value;
  state plpgsql_typed_local_state := 'ready';
  pair plpgsql_typed_local_pair;
BEGIN
  checked := checked + 1;
  state := 'done';
  pair := ROW(checked, state::text);
  RETURN pair.number::text || ':' || pair.label;
END
$$;
CREATE PROCEDURE plpgsql_typed_local_procedure(
  IN input_value plpgsql_typed_local_positive,
  OUT output_state plpgsql_typed_local_state
) LANGUAGE plpgsql AS $$
DECLARE local_state plpgsql_typed_local_state := 'ready';
BEGIN local_state := 'done'; output_state := local_state; END
$$;
CREATE FUNCTION plpgsql_typed_local_trigger() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
  checked plpgsql_typed_local_positive := NEW.value;
  state plpgsql_typed_local_state := 'ready';
BEGIN
  state := 'done';
  INSERT INTO plpgsql_typed_local_audit VALUES (state::text);
  RETURN NEW;
END
$$;
CREATE TABLE plpgsql_typed_local_rows (value integer);
CREATE TRIGGER plpgsql_typed_local_rows_audit
  BEFORE INSERT ON plpgsql_typed_local_rows
  FOR EACH ROW EXECUTE FUNCTION plpgsql_typed_local_trigger();

DO $$
DECLARE state plpgsql_typed_local_state := 'ready';
BEGIN INSERT INTO plpgsql_typed_local_audit VALUES (state::text); END
$$;
SELECT plpgsql_typed_local_value(4::plpgsql_typed_local_positive);
CALL plpgsql_typed_local_procedure(4::plpgsql_typed_local_positive, NULL);
INSERT INTO plpgsql_typed_local_rows VALUES (5);
SELECT label FROM plpgsql_typed_local_audit ORDER BY label;

CREATE FUNCTION plpgsql_typed_local_invalid() RETURNS integer LANGUAGE plpgsql AS $$
DECLARE checked plpgsql_typed_local_positive;
BEGIN checked := 0; RETURN checked; END
$$;
SELECT plpgsql_typed_local_invalid();
INSERT INTO plpgsql_typed_local_rows VALUES (-1);

DROP TABLE plpgsql_typed_local_rows;
DROP TABLE plpgsql_typed_local_audit;
DROP FUNCTION plpgsql_typed_local_value(plpgsql_typed_local_positive);
DROP FUNCTION plpgsql_typed_local_invalid();
DROP FUNCTION plpgsql_typed_local_trigger();
DROP PROCEDURE plpgsql_typed_local_procedure(plpgsql_typed_local_positive);
DROP DOMAIN plpgsql_typed_local_positive;
DROP TYPE plpgsql_typed_local_pair;
DROP TYPE plpgsql_typed_local_state;

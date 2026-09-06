-- Declaration references and contracts are resolved before PL/pgSQL execution.
DROP TABLE IF EXISTS declaration_contract_rows;
DROP TABLE IF EXISTS declaration_contract_audit;
DROP TYPE IF EXISTS declaration_contract_pair;

CREATE TYPE declaration_contract_pair AS (id integer, label text);
CREATE TABLE declaration_contract_rows (id integer, label text);
CREATE TABLE declaration_contract_audit (label text);
INSERT INTO declaration_contract_rows VALUES (7, 'row');

CREATE FUNCTION declaration_contract_value() RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  typed_id declaration_contract_rows.id%TYPE := 7;
  typed_label declaration_contract_pair.label%TYPE := 'pair';
  table_row declaration_contract_rows%ROWTYPE;
  composite_row declaration_contract_pair%ROWTYPE;
  dynamic_row RECORD;
  dynamic_execute RECORD;
  fixed CONSTANT integer := 9;
  required integer NOT NULL := 4;
BEGIN
  SELECT id, label INTO table_row FROM declaration_contract_rows WHERE id = typed_id;
  SELECT id, label INTO composite_row FROM declaration_contract_rows WHERE id = typed_id;
  SELECT id, label INTO dynamic_row FROM declaration_contract_rows WHERE id = typed_id;
  EXECUTE 'SELECT id, label FROM declaration_contract_rows WHERE id = 7' INTO dynamic_execute;
  RETURN typed_id::text || ':' || typed_label || ':' || table_row.label || ':'
    || composite_row.label || ':' || dynamic_row.label || ':' || dynamic_execute.label || ':' || fixed::text
    || ':' || required::text;
END
$$;
CREATE PROCEDURE declaration_contract_procedure() LANGUAGE plpgsql AS $$
DECLARE copied declaration_contract_rows%ROWTYPE;
BEGIN
  SELECT id, label INTO copied FROM declaration_contract_rows WHERE id = 7;
  INSERT INTO declaration_contract_audit VALUES (copied.label);
END
$$;
CREATE FUNCTION declaration_contract_trigger() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE typed_id declaration_contract_rows.id%TYPE := NEW.id;
BEGIN
  INSERT INTO declaration_contract_audit VALUES (typed_id::text);
  RETURN NEW;
END
$$;
CREATE TRIGGER declaration_contract_rows_audit BEFORE INSERT ON declaration_contract_rows
  FOR EACH ROW EXECUTE FUNCTION declaration_contract_trigger();

DO $$
DECLARE
  note declaration_contract_rows.label%TYPE := 'do';
  required integer NOT NULL := 1;
BEGIN
  INSERT INTO declaration_contract_audit VALUES (note);
END
$$;
SELECT declaration_contract_value();
CALL declaration_contract_procedure();
INSERT INTO declaration_contract_rows VALUES (8, 'trigger');
SELECT label FROM declaration_contract_audit ORDER BY label;

CREATE FUNCTION declaration_contract_constant_error() RETURNS integer LANGUAGE plpgsql AS $$
DECLARE fixed CONSTANT integer := 1;
BEGIN fixed := 2; RETURN fixed; END
$$;
SELECT declaration_contract_constant_error();
DO $$ DECLARE required integer NOT NULL; BEGIN NULL; END $$;
DO $$ DECLARE required integer NOT NULL := 1; BEGIN required := NULL; END $$;

DROP TABLE declaration_contract_rows;
DROP TABLE declaration_contract_audit;
DROP FUNCTION declaration_contract_value();
DROP FUNCTION declaration_contract_constant_error();
DROP FUNCTION declaration_contract_trigger();
DROP PROCEDURE declaration_contract_procedure();
DROP TYPE declaration_contract_pair;

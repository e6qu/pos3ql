-- A SQL procedure's own statements send their notices to the client, called
-- directly, from DO, from a trigger, and before an error.
CREATE SCHEMA sql_procedure_notices;
SET search_path = sql_procedure_notices;
CREATE TABLE made(x int);
CREATE PROCEDURE again() LANGUAGE sql
  AS $$CREATE TABLE IF NOT EXISTS made(x int); DROP TABLE IF EXISTS missing$$;
CALL again();
DO $$BEGIN CALL again(); END$$;
CREATE PROCEDURE fails() LANGUAGE sql
  AS $$CREATE TABLE IF NOT EXISTS made(x int); SELECT 1 / 0$$;
CALL fails();
CREATE TABLE c(id int);
CREATE FUNCTION calls() RETURNS trigger LANGUAGE plpgsql
  AS $$BEGIN CALL again(); RETURN NEW; END$$;
CREATE TRIGGER calls BEFORE INSERT ON c FOR EACH ROW EXECUTE FUNCTION calls();
INSERT INTO c VALUES (1), (2);
RESET search_path;
DROP SCHEMA sql_procedure_notices CASCADE;

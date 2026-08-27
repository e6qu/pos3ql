DROP TABLE IF EXISTS plpgsql_contract_log;
DROP PROCEDURE IF EXISTS plpgsql_contract(integer, integer);

CREATE TABLE plpgsql_contract_log(kind text, value integer, note text);
CREATE PROCEDURE plpgsql_contract(IN base integer, INOUT total integer, OUT note text)
  LANGUAGE plpgsql AS $$
DECLARE
  step integer := 0;
BEGIN
  WHILE step < base LOOP
    step := step + 1;
    IF step = 2 THEN
      CONTINUE;
    END IF;
    total := total + step;
  END LOOP;
  note := 'total:' || total;
  INSERT INTO plpgsql_contract_log VALUES ('procedure', total, note);
END
$$;

CALL plpgsql_contract(3, 4, NULL);
DO $$
DECLARE
  value integer := 0;
BEGIN
  FOR value IN 1..3 LOOP
    NULL;
  END LOOP;
  BEGIN
    INSERT INTO plpgsql_contract_log VALUES ('anonymous', value, 'first');
    INSERT INTO plpgsql_contract_log VALUES ('anonymous', value, 'second');
  EXCEPTION WHEN unique_violation THEN
    NULL;
  END;
END
$$;
SELECT kind, value, note FROM plpgsql_contract_log ORDER BY kind, note;
SELECT p.proname, p.prokind, l.lanname
  FROM pg_proc AS p
  JOIN pg_language AS l ON l.oid = p.prolang
 WHERE p.proname = 'plpgsql_contract';

DROP PROCEDURE plpgsql_contract(integer, integer);
DROP TABLE plpgsql_contract_log;

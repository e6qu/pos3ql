-- A value assigned to a PL/pgSQL array element converts as one assigned to a
-- variable: by assignment cast, otherwise through its text form.
DO $$ DECLARE x int[] := '{1,2}'; BEGIN x[1] := true; END $$;
DO $$ DECLARE x int[] := '{1,2}'; b bool := true; BEGIN x[1] := b; END $$;
DO $$ DECLARE x int[] := '{1,2}'; BEGIN x[1] := '2026-01-01'::date; END $$;
CREATE FUNCTION ea87() RETURNS text LANGUAGE plpgsql AS $$
DECLARE x int[] := '{1,2}'; t text[] := '{a}'; n numeric := 2.6;
BEGIN
  x[1] := '7';
  x[2] := n;
  t[2] := 5;
  t[3] := true;
  IF false THEN x[1] := true; END IF;
  RETURN x::text || t::text;
END $$;
SELECT ea87();
DROP ROUTINE ea87() CASCADE;

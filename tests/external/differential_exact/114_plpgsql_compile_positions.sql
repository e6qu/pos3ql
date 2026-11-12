-- PL/pgSQL errors found while compiling a body point at the offending word.
CREATE FUNCTION cp114_dup() RETURNS void LANGUAGE plpgsql AS $$ DECLARE x int; x int; BEGIN END $$;
CREATE FUNCTION cp114_exit() RETURNS void LANGUAGE plpgsql AS $$ BEGIN EXIT nolabel; END $$;
CREATE FUNCTION cp114_continue() RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  LOOP
    CONTINUE missing;
  END LOOP;
END $$;
CREATE FUNCTION cp114_end() RETURNS void LANGUAGE plpgsql AS $$ <<a>> BEGIN END b $$;
CREATE FUNCTION cp114_unlabeled() RETURNS void LANGUAGE plpgsql AS $$ BEGIN END b $$;
CREATE FUNCTION cp114_outside() RETURNS void LANGUAGE plpgsql AS $$ BEGIN EXIT; END $$;
CREATE FUNCTION cp114_block_continue() RETURNS void LANGUAGE plpgsql AS $$
<<blk>> BEGIN
  CONTINUE blk;
END $$;
CREATE FUNCTION cp114_continue_outside() RETURNS void LANGUAGE plpgsql AS $$ BEGIN CONTINUE; END $$;
DO $$ DECLARE y int; y text; BEGIN END $$;

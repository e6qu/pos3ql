-- A PL/pgSQL compiled instance specializes collation and polymorphic inputs.
CREATE SEQUENCE specialization_checks;
CREATE TYPE specialization_choice AS ENUM('a');
CREATE DOMAIN specialization_checked AS specialization_choice CHECK(nextval('specialization_checks')>0);
CREATE FUNCTION pl_collated(input text DEFAULT ('a' COLLATE "C")) RETURNS specialization_checked[]
  LANGUAGE plpgsql AS $$BEGIN RETURN '{a}'::specialization_checked[]; END$$;
CREATE FUNCTION pl_polymorphic(input anyelement) RETURNS specialization_checked[]
  LANGUAGE plpgsql AS $$BEGIN RETURN '{a}'::specialization_checked[]; END$$;
CREATE FUNCTION pl_unreached(input anyelement,choose boolean) RETURNS specialization_checked[]
  LANGUAGE plpgsql AS $$BEGIN IF choose THEN RETURN '{a}'::specialization_checked[]; END IF; RETURN NULL; END$$;
SELECT pl_collated();
SELECT 'first_default',last_value FROM specialization_checks;
SELECT pl_collated('a' COLLATE "C");
SELECT 'explicit_c',last_value FROM specialization_checks;
SELECT pl_collated('a' COLLATE "POSIX");
SELECT 'posix',last_value FROM specialization_checks;
SELECT pl_collated();
SELECT 'default_reused',last_value FROM specialization_checks;
SELECT pl_polymorphic(1);
SELECT pl_polymorphic(2);
SELECT 'integer_reused',last_value FROM specialization_checks;
SELECT pl_polymorphic('a'::text);
SELECT 'separate_text',last_value FROM specialization_checks;
SELECT pl_polymorphic(3);
SELECT 'integer_retained',last_value FROM specialization_checks;
SELECT pl_unreached(1,false);
SELECT pl_unreached('a'::text,false);
SELECT 'unreached_not_checked',last_value FROM specialization_checks;
SELECT pl_unreached(1,true);
SELECT 'reached_integer',last_value FROM specialization_checks;
SELECT pl_unreached('a'::text,true);
SELECT pl_unreached(2,true);
SELECT 'reached_text_and_integer_retained',last_value FROM specialization_checks;
DROP FUNCTION pl_collated(text),pl_polymorphic(anyelement),pl_unreached(anyelement,boolean);
DROP DOMAIN specialization_checked;
DROP TYPE specialization_choice;
DROP SEQUENCE specialization_checks;

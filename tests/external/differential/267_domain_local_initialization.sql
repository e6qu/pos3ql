CREATE SEQUENCE local_initialization_ticks;
CREATE DOMAIN local_required AS integer NOT NULL;
CREATE DOMAIN local_checked AS integer CHECK(VALUE IS NOT NULL);
CREATE TYPE local_pair AS (value integer);
CREATE DOMAIN local_pair_required AS local_pair NOT NULL;
CREATE DOMAIN local_array_required AS integer[] NOT NULL;
CREATE DOMAIN local_nullable AS integer CHECK(VALUE IS NULL OR VALUE > 0);
CREATE FUNCTION local_missing_scalar() RETURNS void LANGUAGE plpgsql AS $$
DECLARE value local_required;
BEGIN PERFORM nextval('local_initialization_ticks'); END
$$;
CREATE FUNCTION local_missing_checked() RETURNS void LANGUAGE plpgsql AS $$
DECLARE value local_checked;
BEGIN PERFORM nextval('local_initialization_ticks'); END
$$;
CREATE FUNCTION local_missing_pair() RETURNS void LANGUAGE plpgsql AS $$
DECLARE value local_pair_required;
BEGIN PERFORM nextval('local_initialization_ticks'); END
$$;
CREATE FUNCTION local_missing_array() RETURNS void LANGUAGE plpgsql AS $$
DECLARE value local_array_required;
BEGIN PERFORM nextval('local_initialization_ticks'); END
$$;
CREATE FUNCTION local_initialization_error(kind integer) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
    IF kind=1 THEN PERFORM local_missing_scalar();
    ELSIF kind=2 THEN PERFORM local_missing_checked();
    ELSIF kind=3 THEN PERFORM local_missing_pair();
    ELSE PERFORM local_missing_array();
    END IF;
    RETURN 'accepted';
EXCEPTION WHEN OTHERS THEN
    RETURN SQLSTATE;
END
$$;
CREATE FUNCTION local_explicit_value() RETURNS integer LANGUAGE plpgsql AS $$
DECLARE value local_required := 7;
BEGIN PERFORM nextval('local_initialization_ticks'); RETURN value; END
$$;
CREATE FUNCTION local_nullable_value() RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE value local_nullable;
BEGIN PERFORM nextval('local_initialization_ticks'); RETURN value IS NULL; END
$$;
SELECT local_initialization_error(1),local_initialization_error(2),
       local_initialization_error(3),local_initialization_error(4),
       local_explicit_value(),local_nullable_value(),
       nextval('local_initialization_ticks');
DROP FUNCTION local_initialization_error(integer);
DROP FUNCTION local_missing_scalar();
DROP FUNCTION local_missing_checked();
DROP FUNCTION local_missing_pair();
DROP FUNCTION local_missing_array();
DROP FUNCTION local_explicit_value();
DROP FUNCTION local_nullable_value();
DROP DOMAIN local_required;
DROP DOMAIN local_checked;
DROP DOMAIN local_pair_required;
DROP DOMAIN local_array_required;
DROP DOMAIN local_nullable;
DROP TYPE local_pair;
DROP SEQUENCE local_initialization_ticks;

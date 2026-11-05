CREATE ROLE subtx_other_user;
CREATE ROLE subtx_definer_owner;
CREATE TABLE subtx_identity_before AS SELECT session_user AS login,current_user AS role_name;
GRANT SELECT ON subtx_identity_before TO subtx_definer_owner;
SET application_name = 'kept';
CREATE FUNCTION subtx_auth_identity() RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN
  BEGIN
    SET SESSION AUTHORIZATION subtx_other_user;
    RAISE EXCEPTION 'restore session authorization';
  EXCEPTION WHEN raise_exception THEN
    RETURN session_user=(SELECT login FROM subtx_identity_before)
       AND current_user=(SELECT role_name FROM subtx_identity_before);
  END;
END$$;
SELECT subtx_auth_identity();
SELECT session_user=login,current_user=role_name FROM subtx_identity_before;
CREATE FUNCTION subtx_definer_identity() RETURNS boolean LANGUAGE plpgsql SECURITY DEFINER AS $$
BEGIN
  BEGIN
    PERFORM set_config('application_name','discarded-definer',false);
    RAISE EXCEPTION 'restore settings inside the owner scope';
  EXCEPTION WHEN raise_exception THEN
    RETURN current_user='subtx_definer_owner'
       AND session_user=(SELECT login FROM subtx_identity_before)
       AND current_setting('application_name')='kept';
  END;
END$$;
ALTER FUNCTION subtx_definer_identity() OWNER TO subtx_definer_owner;
SELECT subtx_definer_identity();
SELECT session_user=login,current_user=role_name,current_setting('application_name')='kept'
FROM subtx_identity_before;

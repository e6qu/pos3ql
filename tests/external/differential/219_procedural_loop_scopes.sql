-- Numeric loop variables are private integers; query and array targets persist.
CREATE FUNCTION loop_private_scope() RETURNS text LANGUAGE plpgsql AS $$
DECLARE value text:='outer'; fixed CONSTANT text:='constant'; total integer:=0; bounds integer:=2;
BEGIN
  FOR value IN bounds..bounds+1 LOOP
    total:=total+value;
    FOR value IN 1..2 LOOP total:=total+value*10; END LOOP;
    total:=total+value;
  END LOOP;
  FOR attempt IN 1..2 LOOP total:=total+attempt; END LOOP;
  FOR fixed IN 1..2 LOOP fixed:=fixed+1; total:=total+fixed; END LOOP;
  RETURN value||':'||fixed||':'||total::text;
END
$$;
CREATE FUNCTION loop_retained_targets() RETURNS text LANGUAGE plpgsql AS $$
DECLARE item integer:=17; result text:='';
BEGIN
  PERFORM 1;
  FOR item IN SELECT 1 UNION ALL SELECT 2 LOOP
    result:=result||CASE WHEN found THEN 't' ELSE 'f' END;
  END LOOP;
  result:=result||':'||item::text||':'||CASE WHEN found THEN 't' ELSE 'f' END;
  FOR item IN EXECUTE 'SELECT 3 UNION ALL SELECT 4' LOOP EXIT; END LOOP;
  result:=result||':'||item::text;
  FOREACH item IN ARRAY ARRAY[5,6] LOOP NULL; END LOOP;
  result:=result||':'||item::text;
  FOREACH item IN ARRAY ARRAY[]::integer[] LOOP NULL; END LOOP;
  result:=result||':'||item::text||':'||CASE WHEN found THEN 't' ELSE 'f' END;
  FOR item IN SELECT 7 WHERE false LOOP NULL; END LOOP;
  RETURN result||':'||coalesce(item::text,'null')||':'||CASE WHEN found THEN 't' ELSE 'f' END;
END
$$;
CREATE FUNCTION loop_output_targets() RETURNS TABLE(item integer) LANGUAGE plpgsql AS $$
BEGIN
  item:=41;
  FOR item IN 1..2 LOOP RETURN NEXT; END LOOP;
  FOR item IN SELECT 1 UNION ALL SELECT 2 LOOP RETURN NEXT; END LOOP;
  FOREACH item IN ARRAY ARRAY[3,4] LOOP RETURN NEXT; END LOOP;
  RETURN NEXT;
END
$$;
CREATE FUNCTION loop_range_errors() RETURNS text LANGUAGE plpgsql AS $$
DECLARE result text:=''; item integer;
BEGIN
  BEGIN FOR item IN NULL::integer..1 LOOP NULL; END LOOP;
  EXCEPTION WHEN SQLSTATE '22004' THEN result:=result||SQLSTATE||','; END;
  BEGIN FOR item IN 1..NULL::integer LOOP NULL; END LOOP;
  EXCEPTION WHEN SQLSTATE '22004' THEN result:=result||SQLSTATE||','; END;
  BEGIN FOR item IN 1..2 BY NULL::integer LOOP NULL; END LOOP;
  EXCEPTION WHEN SQLSTATE '22004' THEN result:=result||SQLSTATE||','; END;
  BEGIN FOR item IN 1..2 BY 0 LOOP NULL; END LOOP;
  EXCEPTION WHEN SQLSTATE '22023' THEN result:=result||SQLSTATE||','; END;
  BEGIN FOR item IN 2147483648::bigint..1 LOOP NULL; END LOOP;
  EXCEPTION WHEN SQLSTATE '22003' THEN result:=result||SQLSTATE||','; END;
  BEGIN FOREACH item IN ARRAY NULL::integer[] LOOP NULL; END LOOP;
  EXCEPTION WHEN SQLSTATE '22004' THEN result:=result||SQLSTATE; END;
  RETURN result;
END
$$;
CREATE FUNCTION loop_empty_records() RETURNS text LANGUAGE plpgsql AS $$
DECLARE item record; result text:='';
BEGIN
  FOR item IN SELECT 1 AS value WHERE false LOOP NULL; END LOOP;
  result:=coalesce(item.value::text,'null');
  FOR item IN EXECUTE 'SELECT 2 AS value WHERE false' LOOP NULL; END LOOP;
  RETURN result||':'||coalesce(item.value::text,'null');
END
$$;
CREATE FUNCTION exception_special_classes() RETURNS text LANGUAGE plpgsql AS $$
DECLARE result text:='';
BEGIN
  BEGIN
    BEGIN RAISE SQLSTATE 'P0004'; EXCEPTION WHEN OTHERS THEN RETURN 'wrong_assert'; END;
  EXCEPTION WHEN assert_failure THEN result:=result||'assert'; END;
  BEGIN
    BEGIN RAISE SQLSTATE '57014'; EXCEPTION WHEN OTHERS THEN RETURN 'wrong_cancel'; END;
  EXCEPTION WHEN query_canceled THEN result:=result||':cancel'; END;
  RETURN result;
END
$$;
-- Execute and inspect the completed results.
SELECT loop_private_scope();
SELECT loop_retained_targets();
SELECT * FROM loop_output_targets();
SELECT loop_range_errors();
SELECT loop_empty_records();
SELECT exception_special_classes();
DROP FUNCTION loop_private_scope(),loop_retained_targets(),loop_output_targets(),loop_range_errors(),loop_empty_records(),exception_special_classes();

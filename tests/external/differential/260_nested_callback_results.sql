CREATE SEQUENCE callback_result_calls;
CREATE TABLE callback_result_audit(call bigint);
CREATE FUNCTION callback_result_rows() RETURNS SETOF text LANGUAGE plpgsql VOLATILE AS $$
DECLARE value text := repeat('x',384);
BEGIN
    INSERT INTO callback_result_audit VALUES(nextval('callback_result_calls'));
    FOR i IN 1..4096 LOOP RETURN NEXT value; END LOOP;
    RETURN;
END$$;
CREATE FUNCTION callback_result_summary() RETURNS text LANGUAGE SQL VOLATILE AS $$
    SELECT count(*)::text || ':' || min(length(value))::text || ':' || max(length(value))::text
    FROM callback_result_rows() AS result(value)
$$;
SELECT callback_result_summary(),callback_result_summary();
SELECT count(*),sum(call),currval('callback_result_calls') FROM callback_result_audit;
SELECT callback_result_summary();
SELECT count(*),sum(call),currval('callback_result_calls') FROM callback_result_audit;
DROP FUNCTION callback_result_summary();
DROP FUNCTION callback_result_rows();
DROP TABLE callback_result_audit;
DROP SEQUENCE callback_result_calls;

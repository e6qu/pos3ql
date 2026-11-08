CREATE SEQUENCE nested_expression_sequence;
CREATE TABLE nested_expression_rows(id integer);
CREATE TABLE nested_expression_audit(label text,n bigint);
CREATE FUNCTION nested_expression_call(label text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE n bigint:=nextval('nested_expression_sequence');
BEGIN
    PERFORM pg_advisory_lock(44);
    INSERT INTO nested_expression_audit VALUES(label,n);
    RETURN n;
END$$;
CREATE FUNCTION nested_expression_trigger() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_advisory_lock(41),nested_expression_call('one'),
        nested_expression_call('two'),pg_advisory_lock(42),nested_expression_call('three');
    RETURN NEW;
END$$;
CREATE TRIGGER nested_expression_trigger AFTER INSERT ON nested_expression_rows
FOR EACH ROW EXECUTE FUNCTION nested_expression_trigger();
-- Waitable write
INSERT INTO nested_expression_rows VALUES(1);
-- Inspect results
SELECT string_agg(label,',' ORDER BY n),sum(n),
    (SELECT last_value FROM nested_expression_sequence),
    pg_advisory_unlock(41),pg_advisory_unlock(41),
    pg_advisory_unlock(44),pg_advisory_unlock(44),pg_advisory_unlock(44),pg_advisory_unlock(44),
    pg_advisory_unlock(42),pg_advisory_unlock(42) FROM nested_expression_audit;

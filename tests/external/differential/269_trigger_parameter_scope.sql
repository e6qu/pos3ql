CREATE TABLE trigger_scope_orders(id integer);
CREATE TABLE trigger_scope_audit(timing text, level_name text, code text, argument text);
CREATE FUNCTION trigger_scope_probe() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE observed text := 'leaked';
BEGIN
    BEGIN
        PERFORM $1;
    EXCEPTION WHEN SQLSTATE '42P02' THEN
        observed := SQLSTATE;
    END;
    INSERT INTO trigger_scope_audit VALUES (TG_WHEN, TG_LEVEL, observed, TG_ARGV[0]);
    IF TG_LEVEL = 'ROW' THEN RETURN NEW; END IF;
    RETURN NULL;
END
$$;
CREATE TRIGGER trigger_scope_before BEFORE INSERT ON trigger_scope_orders
FOR EACH ROW EXECUTE FUNCTION trigger_scope_probe('before argument');
CREATE TRIGGER trigger_scope_after AFTER INSERT ON trigger_scope_orders
FOR EACH ROW EXECUTE FUNCTION trigger_scope_probe('after argument');
CREATE TRIGGER trigger_scope_statement AFTER INSERT ON trigger_scope_orders
FOR EACH STATEMENT EXECUTE FUNCTION trigger_scope_probe('statement argument');
PREPARE trigger_scope_insert(integer) AS INSERT INTO trigger_scope_orders VALUES ($1);
EXECUTE trigger_scope_insert(7);
EXECUTE trigger_scope_insert(8);
SELECT count(*),
    sum(CASE WHEN timing = 'BEFORE' AND level_name = 'ROW' THEN 1 ELSE 0 END),
    sum(CASE WHEN timing = 'AFTER' AND level_name = 'ROW' THEN 1 ELSE 0 END),
    sum(CASE WHEN timing = 'AFTER' AND level_name = 'STATEMENT' THEN 1 ELSE 0 END),
    bool_and(code IS NOT NULL AND code = '42P02'),
    bool_and(argument IS NOT NULL AND argument = CASE
        WHEN timing = 'BEFORE' THEN 'before argument'
        WHEN level_name = 'ROW' THEN 'after argument'
        ELSE 'statement argument' END),
    (SELECT sum(id) FROM trigger_scope_orders)
FROM trigger_scope_audit;
DEALLOCATE trigger_scope_insert;
DROP TABLE trigger_scope_orders;
DROP TABLE trigger_scope_audit;
DROP FUNCTION trigger_scope_probe();

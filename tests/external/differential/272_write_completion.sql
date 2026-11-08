CREATE TABLE completion_orders(id integer PRIMARY KEY);
CREATE SEQUENCE completion_order;
CREATE TABLE completion_audit(ordinal bigint, timing text, operation text);
CREATE FUNCTION completion_probe() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO completion_audit VALUES (nextval('completion_order'), TG_WHEN, TG_OP);
    RETURN NULL;
END
$$;
CREATE TRIGGER completion_before BEFORE INSERT OR UPDATE OR DELETE ON completion_orders
FOR EACH STATEMENT EXECUTE FUNCTION completion_probe();
CREATE TRIGGER completion_after AFTER INSERT OR UPDATE OR DELETE ON completion_orders
FOR EACH STATEMENT EXECUTE FUNCTION completion_probe();
INSERT INTO completion_orders SELECT 1 WHERE false;
UPDATE completion_orders SET id = 99 WHERE false RETURNING id;
DELETE FROM completion_orders WHERE false RETURNING id;
MERGE INTO completion_orders AS target USING (VALUES (1)) AS source(id)
ON target.id = source.id
WHEN MATCHED THEN DO NOTHING
WHEN NOT MATCHED THEN DO NOTHING;
MERGE INTO completion_orders AS target USING (VALUES (1)) AS source(id)
ON target.id = source.id
WHEN MATCHED THEN UPDATE SET id = source.id
WHEN NOT MATCHED THEN INSERT VALUES (source.id);
INSERT INTO completion_orders VALUES (1)
ON CONFLICT (id) DO UPDATE SET id = EXCLUDED.id;
WITH added AS (INSERT INTO completion_orders VALUES (2) RETURNING id)
SELECT id FROM added;
SELECT count(*),
    sum(CASE WHEN operation = 'INSERT' THEN 1 ELSE 0 END),
    sum(CASE WHEN operation = 'UPDATE' THEN 1 ELSE 0 END),
    sum(CASE WHEN operation = 'DELETE' THEN 1 ELSE 0 END),
    bool_and(timing || ' ' || operation = CASE ordinal
        WHEN 1 THEN 'BEFORE INSERT' WHEN 2 THEN 'AFTER INSERT'
        WHEN 3 THEN 'BEFORE UPDATE' WHEN 4 THEN 'AFTER UPDATE'
        WHEN 5 THEN 'BEFORE DELETE' WHEN 6 THEN 'AFTER DELETE'
        WHEN 7 THEN 'BEFORE INSERT' WHEN 8 THEN 'BEFORE UPDATE'
        WHEN 9 THEN 'AFTER UPDATE' WHEN 10 THEN 'AFTER INSERT'
        WHEN 11 THEN 'BEFORE INSERT' WHEN 12 THEN 'BEFORE UPDATE'
        WHEN 13 THEN 'AFTER UPDATE' WHEN 14 THEN 'AFTER INSERT'
        WHEN 15 THEN 'BEFORE INSERT' WHEN 16 THEN 'AFTER INSERT'
        ELSE 'unexpected trigger' END),
    (SELECT sum(id) FROM completion_orders), nextval('completion_order')
FROM completion_audit;
DROP TABLE completion_orders;
DROP TABLE completion_audit;
DROP FUNCTION completion_probe();
DROP SEQUENCE completion_order;

CREATE TABLE mixed_orders(id integer PRIMARY KEY, amount integer);
INSERT INTO mixed_orders VALUES (1,10), (2,20), (4,40);
CREATE SEQUENCE mixed_trigger_order;
CREATE TABLE mixed_audit(
    ordinal bigint, operation text, row_id integer,
    old_total integer, new_total integer, old_count integer, new_count integer
);
CREATE FUNCTION mixed_transition_probe() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    affected integer;
    old_total integer := 0;
    new_total integer := 0;
    old_count integer := 0;
    new_count integer := 0;
BEGIN
    IF TG_LEVEL = 'STATEMENT' THEN
        affected := 0;
    ELSIF TG_OP = 'INSERT' THEN
        affected := NEW.id;
    ELSE
        affected := OLD.id;
    END IF;
    IF TG_OP <> 'INSERT' THEN
        SELECT sum(amount), count(*) INTO old_total, old_count FROM old_rows;
    END IF;
    IF TG_OP <> 'DELETE' THEN
        SELECT sum(amount), count(*) INTO new_total, new_count FROM new_rows;
    END IF;
    INSERT INTO mixed_audit VALUES (
        nextval('mixed_trigger_order'), TG_OP, affected,
        old_total, new_total, old_count, new_count
    );
    RETURN NULL;
END
$$;
CREATE TRIGGER mixed_insert AFTER INSERT ON mixed_orders
REFERENCING NEW TABLE AS new_rows FOR EACH ROW
EXECUTE FUNCTION mixed_transition_probe();
CREATE TRIGGER mixed_update AFTER UPDATE ON mixed_orders
REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows FOR EACH ROW
EXECUTE FUNCTION mixed_transition_probe();
CREATE TRIGGER mixed_delete AFTER DELETE ON mixed_orders
REFERENCING OLD TABLE AS old_rows FOR EACH ROW
EXECUTE FUNCTION mixed_transition_probe();
CREATE TRIGGER mixed_insert_statement AFTER INSERT ON mixed_orders
REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT
EXECUTE FUNCTION mixed_transition_probe();
CREATE TRIGGER mixed_update_statement AFTER UPDATE ON mixed_orders
REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows FOR EACH STATEMENT
EXECUTE FUNCTION mixed_transition_probe();
CREATE TRIGGER mixed_delete_statement AFTER DELETE ON mixed_orders
REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT
EXECUTE FUNCTION mixed_transition_probe();
INSERT INTO mixed_orders VALUES (3,30), (1,11)
ON CONFLICT (id) DO UPDATE SET amount = EXCLUDED.amount;
MERGE INTO mixed_orders AS target
USING (VALUES (3,300,'delete'), (2,200,'update'), (5,50,'insert'))
    AS source(id, amount, action)
ON target.id = source.id
WHEN MATCHED AND source.action = 'delete' THEN DELETE
WHEN MATCHED THEN UPDATE SET amount = source.amount
WHEN NOT MATCHED THEN INSERT (id, amount) VALUES (source.id, source.amount);
SELECT count(*),
    sum(CASE WHEN operation = 'INSERT' THEN 1 ELSE 0 END),
    sum(CASE WHEN operation = 'UPDATE' THEN 1 ELSE 0 END),
    sum(CASE WHEN operation = 'DELETE' THEN 1 ELSE 0 END),
    bool_and(old_total IS NOT NULL AND new_total IS NOT NULL AND CASE
        WHEN operation = 'INSERT' AND row_id = 3 THEN
            old_total = 0 AND new_total = 30 AND old_count = 0 AND new_count = 1
        WHEN operation = 'UPDATE' AND row_id = 1 THEN
            old_total = 10 AND new_total = 11 AND old_count = 1 AND new_count = 1
        WHEN operation = 'DELETE' AND row_id = 3 THEN
            old_total = 30 AND new_total = 0 AND old_count = 1 AND new_count = 0
        WHEN operation = 'UPDATE' AND row_id = 2 THEN
            old_total = 20 AND new_total = 200 AND old_count = 1 AND new_count = 1
        WHEN operation = 'INSERT' AND row_id = 5 THEN
            old_total = 0 AND new_total = 50 AND old_count = 0 AND new_count = 1
        WHEN operation = 'INSERT' AND row_id = 0 AND ordinal <= 4 THEN
            old_total = 0 AND new_total = 30 AND old_count = 0 AND new_count = 1
        WHEN operation = 'UPDATE' AND row_id = 0 AND ordinal <= 4 THEN
            old_total = 10 AND new_total = 11 AND old_count = 1 AND new_count = 1
        WHEN operation = 'INSERT' AND row_id = 0 AND ordinal > 4 THEN
            old_total = 0 AND new_total = 50 AND old_count = 0 AND new_count = 1
        WHEN operation = 'UPDATE' AND row_id = 0 AND ordinal > 4 THEN
            old_total = 20 AND new_total = 200 AND old_count = 1 AND new_count = 1
        WHEN operation = 'DELETE' AND row_id = 0 THEN
            old_total = 30 AND new_total = 0 AND old_count = 1 AND new_count = 0
        ELSE false END),
    (SELECT operation = 'INSERT' AND row_id = 3 FROM mixed_audit WHERE ordinal = 1)
        AND (SELECT operation = 'UPDATE' AND row_id = 1 FROM mixed_audit WHERE ordinal = 2),
    (SELECT sum(amount) FROM mixed_orders)
FROM mixed_audit;
DROP TABLE mixed_orders;
DROP TABLE mixed_audit;
DROP FUNCTION mixed_transition_probe();
DROP SEQUENCE mixed_trigger_order;

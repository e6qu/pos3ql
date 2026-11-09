CREATE DOMAIN transition_amount AS integer CHECK (VALUE > 0);
CREATE TYPE transition_state AS ENUM ('pending', 'done');
CREATE TYPE transition_record AS (code integer);
CREATE TABLE transition_orders(
    id integer,
    amount transition_amount,
    state transition_state,
    details transition_record,
    amounts transition_amount[]
);
CREATE TABLE transition_type_audit(operation text, side text, kinds text, null_input boolean);
CREATE FUNCTION transition_new_types() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO transition_type_audit
    SELECT TG_OP, 'new',
        pg_typeof(amount)::text || ':' || pg_typeof(state)::text || ':' ||
        pg_typeof(details)::text || ':' || pg_typeof(amounts)::text,
        amount IS NULL AND state IS NULL AND details IS NULL AND amounts IS NULL
    FROM new_rows;
    RETURN NULL;
END
$$;
CREATE FUNCTION transition_old_types() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO transition_type_audit
    SELECT TG_OP, 'old',
        pg_typeof(amount)::text || ':' || pg_typeof(state)::text || ':' ||
        pg_typeof(details)::text || ':' || pg_typeof(amounts)::text,
        amount IS NULL AND state IS NULL AND details IS NULL AND amounts IS NULL
    FROM old_rows;
    RETURN NULL;
END
$$;
CREATE TRIGGER transition_insert_new AFTER INSERT ON transition_orders
REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT
EXECUTE FUNCTION transition_new_types();
CREATE TRIGGER transition_update_new AFTER UPDATE ON transition_orders
REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT
EXECUTE FUNCTION transition_new_types();
CREATE TRIGGER transition_update_old AFTER UPDATE ON transition_orders
REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT
EXECUTE FUNCTION transition_old_types();
CREATE TRIGGER transition_delete_old AFTER DELETE ON transition_orders
REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT
EXECUTE FUNCTION transition_old_types();
INSERT INTO transition_orders VALUES
    (1, 1, 'pending', ROW(11)::transition_record, ARRAY[1,2]::transition_amount[]),
    (2, NULL, NULL, NULL, NULL);
UPDATE transition_orders SET amount = amount + 10;
DELETE FROM transition_orders;
SELECT count(*),
    sum(CASE WHEN operation = 'INSERT' AND side = 'new' THEN 1 ELSE 0 END),
    sum(CASE WHEN operation = 'UPDATE' AND side = 'new' THEN 1 ELSE 0 END),
    sum(CASE WHEN operation = 'UPDATE' AND side = 'old' THEN 1 ELSE 0 END),
    sum(CASE WHEN operation = 'DELETE' AND side = 'old' THEN 1 ELSE 0 END),
    sum(CASE WHEN null_input THEN 1 ELSE 0 END),
    bool_and(kinds IS NOT NULL AND
        kinds = 'transition_amount:transition_state:transition_record:transition_amount[]')
FROM transition_type_audit;
DROP TABLE transition_orders;
DROP TABLE transition_type_audit;
DROP FUNCTION transition_new_types();
DROP FUNCTION transition_old_types();
DROP DOMAIN transition_amount;
DROP TYPE transition_state;
DROP TYPE transition_record;

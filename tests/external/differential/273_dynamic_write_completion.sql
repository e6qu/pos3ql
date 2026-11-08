CREATE SEQUENCE dynamic_held_bind;
CREATE SEQUENCE dynamic_held_child;
CREATE TABLE dynamic_held_orders(id integer);
CREATE TABLE dynamic_held_stock(id integer, amount integer);
CREATE TABLE dynamic_held_audit(stage text,id integer,n bigint,note text,missing boolean,declared_type text);
INSERT INTO dynamic_held_stock VALUES(1,0),(2,0);
CREATE FUNCTION dynamic_held_row() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE n bigint:=nextval('dynamic_held_child'); note text:=repeat('c',8192);
BEGIN
    INSERT INTO dynamic_held_audit VALUES('child-before',NEW.id,n,note,NULL,NULL);
    PERFORM pg_advisory_xact_lock(43);
    INSERT INTO dynamic_held_audit VALUES('child-after',NEW.id,n,note||'!',NULL,NULL);
    RETURN NEW;
END$$;
CREATE TRIGGER dynamic_held_row AFTER UPDATE ON dynamic_held_stock
FOR EACH ROW EXECUTE FUNCTION dynamic_held_row();
CREATE FUNCTION dynamic_held_parent() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE selected record; changed bigint; note text:=repeat('p',8192);
BEGIN
    INSERT INTO dynamic_held_audit VALUES('parent-before',NEW.id,0,note,NULL,NULL);
    PERFORM 1 WHERE false;
    EXECUTE 'UPDATE dynamic_held_stock SET amount=amount+1 WHERE id=$1
        RETURNING id,repeat(''r'',8192)||id AS note,NULL::integer AS missing'
        INTO STRICT selected USING nextval('dynamic_held_bind');
    GET DIAGNOSTICS changed=ROW_COUNT;
    IF changed<>1 OR FOUND THEN RAISE EXCEPTION 'dynamic diagnostics changed'; END IF;
    INSERT INTO dynamic_held_audit VALUES('parent-after',selected.id,changed,
        selected.note||note,selected.missing IS NULL,pg_typeof(selected.missing)::text);
    RETURN NEW;
END$$;
CREATE TRIGGER dynamic_held_parent AFTER INSERT ON dynamic_held_orders
FOR EACH ROW EXECUTE FUNCTION dynamic_held_parent();
-- Waitable write
INSERT INTO dynamic_held_orders VALUES(10) RETURNING id;
-- Inspect results
SELECT (SELECT sum(amount) FROM dynamic_held_stock),
    (SELECT last_value FROM dynamic_held_bind),(SELECT last_value FROM dynamic_held_child),
    (SELECT count(*) FROM dynamic_held_audit WHERE stage='child-before'),
    (SELECT count(*) FROM dynamic_held_audit WHERE stage='child-after'),
    (SELECT count(*) FROM dynamic_held_audit WHERE stage='parent-before'),
    (SELECT count(*) FROM dynamic_held_audit WHERE stage='parent-after'),
    length(note),id,n,missing,declared_type FROM dynamic_held_audit WHERE stage='parent-after';

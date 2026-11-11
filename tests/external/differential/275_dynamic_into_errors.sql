CREATE SEQUENCE into_edge_sequence;
CREATE DOMAIN into_edge_positive AS integer CHECK(VALUE>0);
CREATE TABLE into_edge_held_values(id into_edge_positive);
INSERT INTO into_edge_held_values VALUES(9);
DECLARE into_held_cursor CURSOR WITH HOLD FOR SELECT * FROM into_edge_held_values;
CREATE TABLE into_edge_orders(id integer);
CREATE TABLE into_edge_stock(id integer,amount integer);
CREATE TABLE into_edge_audit(stage text,n bigint,note text,error_code text,missing boolean,declared_type text);
INSERT INTO into_edge_stock VALUES(1,0),(2,0);
CREATE FUNCTION into_edge_child() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE n bigint:=nextval('into_edge_sequence'); note text:=repeat('c',8192);
BEGIN
    INSERT INTO into_edge_audit VALUES('child-before',n,note,NULL,NULL,NULL);
    PERFORM pg_advisory_xact_lock(43);
    INSERT INTO into_edge_audit VALUES('child-after',n,note||'!',NULL,NULL,NULL);
    RETURN NEW;
END$$;
CREATE TRIGGER into_edge_child AFTER UPDATE ON into_edge_stock
FOR EACH ROW EXECUTE FUNCTION into_edge_child();
CREATE FUNCTION into_edge_utility() RETURNS void LANGUAGE plpgsql AS $$
DECLARE utility record;
BEGIN
    EXECUTE 'SHOW TimeZone' INTO utility;
    IF utility."TimeZone"<>current_setting('TimeZone') THEN
        RAISE EXCEPTION 'SHOW record description changed';
    END IF;
    EXECUTE 'SHOW ALL' INTO utility;
    IF utility.name IS NULL OR utility.setting IS NULL OR pg_typeof(utility.description)<>'text'::regtype THEN
        RAISE EXCEPTION 'SHOW ALL record description changed';
    END IF;
    EXECUTE 'EXPLAIN SELECT 1' INTO utility;
    IF utility."QUERY PLAN" IS NULL OR pg_typeof(utility."QUERY PLAN")<>'text'::regtype THEN
        RAISE EXCEPTION 'EXPLAIN record description changed';
    END IF;
END$$;
CREATE FUNCTION into_edge_empty() RETURNS void LANGUAGE plpgsql AS $$
DECLARE utility record;
BEGIN
    EXECUTE 'SELECT NULL::into_edge_positive AS id WHERE false' INTO utility;
    IF utility.id IS NOT NULL OR pg_typeof(utility.id)<>'into_edge_positive'::regtype THEN
        RAISE EXCEPTION 'empty SELECT domain description changed';
    END IF;
    SELECT NULL::into_edge_positive AS id INTO utility WHERE false;
    IF utility.id IS NOT NULL OR pg_typeof(utility.id)<>'into_edge_positive'::regtype THEN
        RAISE EXCEPTION 'empty static SELECT domain description changed';
    END IF;
    EXECUTE 'UPDATE into_edge_stock SET amount=amount WHERE false RETURNING NULL::into_edge_positive AS id'
        INTO utility;
    IF utility.id IS NOT NULL OR pg_typeof(utility.id)<>'into_edge_positive'::regtype THEN
        RAISE EXCEPTION 'empty RETURNING domain description changed';
    END IF;
END$$;
CREATE FUNCTION into_edge_fetch() RETURNS void LANGUAGE plpgsql AS $$
DECLARE utility record; held_cursor refcursor:='into_empty_cursor'; saved_cursor refcursor:='into_held_cursor';
BEGIN
    EXECUTE 'FETCH NEXT FROM into_held_cursor' INTO utility;
    IF utility.id<>9 OR pg_typeof(utility.id)<>'into_edge_positive'::regtype THEN
        RAISE EXCEPTION 'held FETCH domain description changed';
    END IF;
    EXECUTE 'FETCH NEXT FROM into_held_cursor' INTO utility;
    IF utility.id IS NOT NULL OR pg_typeof(utility.id)<>'into_edge_positive'::regtype THEN
        RAISE EXCEPTION 'empty held FETCH domain description changed';
    END IF;
    CLOSE saved_cursor;
    OPEN held_cursor FOR SELECT NULL::into_edge_positive AS id,NULL::text AS payload WHERE false;
    EXECUTE 'FETCH NEXT FROM into_empty_cursor' INTO utility;
    IF utility.id IS NOT NULL OR utility.payload IS NOT NULL
        OR pg_typeof(utility.id)<>'into_edge_positive'::regtype THEN
        RAISE EXCEPTION 'empty FETCH record description changed';
    END IF;
    CLOSE held_cursor;
    OPEN held_cursor FOR SELECT 5::into_edge_positive AS id;
    FETCH held_cursor INTO utility;
    IF utility.id<>5 OR pg_typeof(utility.id)<>'into_edge_positive'::regtype THEN
        RAISE EXCEPTION 'FETCH domain description changed';
    END IF;
    CLOSE held_cursor;
    OPEN held_cursor FOR SELECT NULL::into_edge_positive AS id
        UNION ALL SELECT NULL::into_edge_positive WHERE false;
    EXECUTE 'FETCH NEXT FROM into_empty_cursor' INTO utility;
    IF utility.id IS NOT NULL OR pg_typeof(utility.id)<>'into_edge_positive'::regtype THEN
        RAISE EXCEPTION 'set query FETCH domain description changed';
    END IF;
    CLOSE held_cursor;

END$$;
CREATE FUNCTION into_edge_parent() RETURNS trigger LANGUAGE plpgsql AS $$
#print_strict_params on
DECLARE invalid record; empty_row record;
    error_code text; strict_code text; empty_code text; strict_detail text;
    changed bigint; strict_count bigint; note text:=repeat('p',8192);
BEGIN
    INSERT INTO into_edge_audit VALUES('parent-before',0,note,NULL,NULL,NULL);
    PERFORM 1 WHERE false;
    BEGIN
        EXECUTE 'UPDATE into_edge_stock SET amount=amount+1 WHERE id=1' INTO invalid;
        RAISE EXCEPTION 'INTO without a result was accepted';
    EXCEPTION WHEN SQLSTATE '42601' THEN
        GET STACKED DIAGNOSTICS error_code=RETURNED_SQLSTATE;
        GET DIAGNOSTICS changed=ROW_COUNT;
    END;
    EXECUTE 'SELECT 77 AS kept' INTO invalid;
    BEGIN
        EXECUTE 'UPDATE into_edge_stock SET amount=amount+$1 RETURNING id' INTO STRICT invalid
            USING 1;
        RAISE EXCEPTION 'STRICT accepted multiple rows';
    EXCEPTION WHEN SQLSTATE 'P0003' THEN
        GET STACKED DIAGNOSTICS strict_code=RETURNED_SQLSTATE, strict_detail=PG_EXCEPTION_DETAIL;
        GET DIAGNOSTICS strict_count=ROW_COUNT;
    END;
    BEGIN
        EXECUTE 'SELECT 0::integer AS kept WHERE false' INTO STRICT invalid;
        RAISE EXCEPTION 'STRICT accepted no rows';
    EXCEPTION WHEN SQLSTATE 'P0002' THEN
        GET STACKED DIAGNOSTICS empty_code=RETURNED_SQLSTATE;
    END;
    EXECUTE 'SELECT NULL::integer AS id,NULL::text AS payload WHERE false' INTO empty_row;
    IF changed<>1 OR strict_count<>2 OR invalid.kept<>77 OR FOUND
        OR empty_row.id IS NOT NULL OR empty_row.payload IS NOT NULL THEN
        RAISE EXCEPTION 'dynamic INTO error or empty result changed';
    END IF;
    PERFORM into_edge_utility();
    PERFORM into_edge_empty();
    PERFORM into_edge_fetch();
    PERFORM 1 WHERE false;
    INSERT INTO into_edge_audit VALUES('parent-after',changed,note||'!',error_code||'/'||strict_code||'/'||empty_code||'/'||strict_detail,
        empty_row.id IS NULL,pg_typeof(empty_row.id)::text);
    RETURN NEW;
END$$;
CREATE TRIGGER into_edge_parent AFTER INSERT ON into_edge_orders
FOR EACH ROW EXECUTE FUNCTION into_edge_parent();
-- Waitable write
INSERT INTO into_edge_orders VALUES(10) RETURNING id;
-- Inspect results
SELECT (SELECT sum(amount) FROM into_edge_stock),(SELECT last_value FROM into_edge_sequence),
    (SELECT count(*) FROM into_edge_audit WHERE stage LIKE 'child-%'),
    (SELECT count(*) FROM into_edge_audit WHERE stage='parent-before'),
    (SELECT count(*) FROM into_edge_audit WHERE stage='parent-after'),
    error_code,n,length(note),missing,declared_type FROM into_edge_audit WHERE stage='parent-after';

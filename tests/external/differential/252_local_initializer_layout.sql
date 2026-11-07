CREATE SEQUENCE local_initializer_steps;
CREATE FUNCTION local_initializer_layout(seed integer, OUT total integer, OUT label text)
LANGUAGE plpgsql AS $$
DECLARE first constant integer := nextval('local_initializer_steps')::integer;
    second integer NOT NULL := seed + first;
    suffix text NOT NULL := 'ok';
    plain integer;
    was_null boolean := plain IS NULL;
BEGIN
    FOR second IN 1..3 LOOP total := coalesce(total,0) + second; END LOOP;
    total := total + second;
    label := first::text || ':' || suffix || ':' || was_null::text;
    RETURN;
END$$;
SELECT total,label FROM local_initializer_layout(10);
SELECT total,label FROM local_initializer_layout(20);
SELECT last_value FROM local_initializer_steps;
DROP FUNCTION local_initializer_layout(integer);
DROP SEQUENCE local_initializer_steps;

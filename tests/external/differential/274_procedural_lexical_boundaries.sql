SET client_min_messages=error;
CREATE FUNCTION lexical_boundaries() RETURNS text LANGUAGE plpgsql AS $function$
DECLARE
    declaration text DEFAULT $value$begin, into, using, then, loop := =$value$;
    selected record;
    marker text:='none';
    total integer:=0;
    "row=count" bigint:=0;
BEGIN
    EXECUTE /* INTO USING */ $command$SELECT $payload$INTO, USING, THEN, LOOP$payload$ AS note,
        $1 AS bound,$2 AS item$command$
        INTO STRICT selected USING $argument$first,second$argument$,/* INTO, /* USING */ THEN */(ARRAY['x','y'])[1];
    GET DIAGNOSTICS "row=count" /* := */ := ROW_COUNT;
    IF($condition$THEN; LOOP$condition$='THEN; LOOP')THEN marker:='branch';
    ELSE RAISE EXCEPTION 'quoted control condition changed'; END IF;
    FOR n IN length($lower$..$lower$)/* BY .. */..length($upper$loop, by$upper$) BY 2 LOOP
        total:=total+n;
    END LOOP;
    RAISE NOTICE $format$using, %$format$,selected.note USING DETAIL=$detail$detail, using$detail$;
    RETURN(CASE WHEN declaration=$value$begin, into, using, then, loop := =$value$
        THEN 'ok' ELSE 'bad' END ||'|'||marker||'|'||total::text||'|'||selected.note
        ||'|'||selected.bound||'|'||selected.item||'|'||"row=count"::text);
END$function$;
SELECT lexical_boundaries();

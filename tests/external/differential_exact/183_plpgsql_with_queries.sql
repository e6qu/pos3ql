-- PL/pgSQL RETURN QUERY, SELECT INTO and FOR loops read their ordinary WITH queries.
create function rq183a() returns setof int language plpgsql as $$ begin return query with c as (select 1 union all select 2) select * from c; end $$;
select * from rq183a();
create function rq183b(x int) returns int language plpgsql as $$ declare n int; begin with c as materialized (select g from generate_series(1, 4) g) select count(*) into n from c where g > x; return n; end $$;
select rq183b(1);
do $$ declare r record; t int := 0; begin for r in with c as (select g from generate_series(1, 3) g) select g from c loop t := t + r.g; end loop; raise notice 'total %', t; end $$;
drop function rq183a(); drop function rq183b(int);

-- CONSTANT assignment targets, set-operation leaves and named composite array operands are positioned and named as PostgreSQL does.
do $$ declare c constant int := 1; begin c := 2; end $$;
do $$ declare c constant int := 1; begin
  c[1] := 2; end $$;
do $$ <<b>> declare c constant int := 1; begin b.c := 2; end $$;
do $$ declare c constant int := 1; begin select 1 into c; end $$;
do $$ declare c constant int := 1; begin get diagnostics c = row_count; end $$;
do $$ declare c constant int := 1; begin for c in select 1 loop end loop; end $$;
do $$ declare a int; a int; begin null; end $$;
create function cf192() returns void language plpgsql as $$ declare c constant int := 1; begin
  select 1 into c; end $$;
select x from (select 1) s union select 1;
select 1 union select x;
select 1 union all select 2 union select y;
with w as (select 1 as a) select a from w union select z;
create type ct192 as (a int, b text);
select array[row(1, 'x')::ct192] + 1;
select array[row(1, 'x')::ct192] < array[1];
drop type ct192;

-- A statement's catalog scans read the catalog as the statement began, even
-- after a routine it calls creates tables; a volatile routine's own queries
-- see the change.
create function mk347(i int) returns int language plpgsql as $$
begin execute format('create table made347_%s (a int)', i); return i; end $$;
select mk347(g), (select count(*) from pg_class where relname like 'made347%') from generate_series(1, 3) g;
select count(*) from pg_class where relname like 'made347%';
create view made347_view as select relname from pg_class where relname like 'made347%';
select mk347(g), (select count(*) from made347_view) from generate_series(4, 5) g;
create function cntv347() returns bigint language sql volatile
  as $$ select count(*) from pg_class where relname like 'made347%' $$;
select mk347(g), cntv347() from generate_series(6, 7) g;
select relname from pg_class where relname like 'made347%' order by 1;
drop view made347_view;
drop table made347_1, made347_2, made347_3, made347_4, made347_5, made347_6, made347_7;
drop function mk347(int), cntv347();

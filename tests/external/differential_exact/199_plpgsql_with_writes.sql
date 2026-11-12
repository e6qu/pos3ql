-- PL/pgSQL writes, EXECUTE and FOR queries with WITH queries that read variables or write.
create table wt191(a int, b int);
insert into wt191 values (1, 10), (2, 20);
create function w191() returns int language plpgsql as $$
declare n int := 5;
begin
  with s as (select a from wt191 where a = 1) update wt191 set b = b + n from s where wt191.a = s.a;
  with s as (insert into wt191 values (3, n) returning a) insert into wt191 select a + 10, n from s;
  with d as (delete from wt191 where a = 13 returning a) delete from wt191 where a in (select a - 10 from d);
  return (select count(*) from wt191);
end $$;
select w191();
select * from wt191 order by a;
create function w191b() returns setof int language plpgsql as $$
declare n int := 2; r record; x int;
begin
  for r in with s as (select a from wt191 where a <= n) select a from s order by a loop return next r.a; end loop;
  for r in with i as (insert into wt191 values (7, 7) returning a) select a from i loop return next r.a; end loop;
  return query execute 'with s as (select a from wt191 where a > $1) select a from s order by a' using n;
  for r in execute 'with s as (select a from wt191) select max(a) as m from s' loop return next r.m; end loop;
  execute 'with i as (insert into wt191 values (99, 0) returning a) select a from i' into x; return next x;
  execute 'with s as (select 1 as a) insert into wt191 select a + 40, $1 from s' using n;
  execute 'with d as (delete from wt191 where a = 41 returning a) update wt191 set b = 8 where a in (select a - 40 from d)';
end $$;
select * from w191b();
select * from wt191 order by a;
do $$ declare x int; begin
  insert into wt191 values (77, 0);
  with d as (delete from wt191 where a = 77 returning a) select count(*) into x from d;
  raise notice 'deleted %', x;
  with i as (insert into wt191 values (79, 0) returning a) select count(*) into x from wt191 where a = 79;
  raise notice 'seen %', x;
  with i as (insert into wt191 values (80, 0) returning a) update wt191 set b = b + 1 where a >= 79;
  with i as (update wt191 set b = 5 where a = 80 returning a) update wt191 set b = b + 1 where a in (select a from i);
end $$;
select * from wt191 where a >= 79 order by a;
create table wlog191(a int);
create function wtrg191() returns trigger language plpgsql as $$
begin
  with s as (select new.a as a) insert into wlog191 select a from s;
  return new;
end $$;
create trigger wt191_t before insert on wt191 for each row execute function wtrg191();
insert into wt191 values (50, 0);
select * from wlog191;
create or replace function wtrg191() returns trigger language plpgsql as $$
declare x int;
begin
  with s as (insert into wlog191 values (new.a) returning a) insert into wlog191 select a + 1 from s;
  with d as (delete from wlog191 where a = new.a + 1 returning a) select count(*) into x from d;
  with u as (update wlog191 set a = a + 100 where a = new.a returning a) select 1 into x;
  raise notice 'deleted %', x;
  return new;
end $$;
insert into wt191 values (60, 0), (70, 0);
select * from wlog191 order by a;
drop table wt191, wlog191; drop function w191(); drop function w191b(); drop function wtrg191();

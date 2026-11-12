-- #variable_conflict use_variable reads the variable wherever a column of the
-- same name would bind: aggregates, grouping keys, DISTINCT ON, window keys,
-- subqueries, DML, triggers and cursor queries; use_column and error as PostgreSQL.
create table vt185a(a int, b int, c text);
insert into vt185a values (1,10,'x'),(2,20,'y'),(3,30,'x'),(4,40,'z');
create function f185a() returns table(r text) language plpgsql as $$
#variable_conflict use_variable
declare a int := 2; c text := 'x';
begin
  return query select format('%s', sum(a)) from vt185a;
  return query select format('%s|%s', a, count(*)) from vt185a group by a;
  return query select format('%s', b) from vt185a where a = 2 order by b;
  return query select format('%s', b) from vt185a order by a, b desc limit 2;
  return query select distinct on (c) format('%s/%s', c, b) from vt185a order by c, b;
  return query select format('%s', max(b) filter (where c = 'x')) from vt185a;
  return query select format('%s', row_number() over (partition by c order by b)) from vt185a;
  return query select format('%s', (select max(a) from vt185a v2)) from vt185a limit 1;
  return query select format('%s', vt185a.a) from vt185a where vt185a.a = a;
  return query select format('%s', x) from (select a + b as x from vt185a) s order by 1 limit 1;
  return query select format('%s', string_agg(c, ',' order by c)) from vt185a;
  return query select format('%s', count(distinct c)) from vt185a;
  return query select format('%s', a) from vt185a group by b having count(*) > 0 order by b limit 1;
end $$;
select * from f185a();
create function f185b() returns int language plpgsql as $$
#variable_conflict use_variable
declare b int := 5;
begin
  update vt185a set b = b + 1 where a = 1;
  insert into vt185a(a,b,c) values (9, b, 'w');
  delete from vt185a where b = 5 and a <> 9;
  return (select vt185a.b from vt185a where vt185a.a = 1);
end $$;
select f185b();
select * from vt185a order by a;

create table vt185b(a int, b numeric(6,2), c text);
insert into vt185b values (1,10,'x'),(2,20,'y'),(3,30,'x');
create type mood185 as enum ('sad','ok','happy');
create table mt185(m mood185, a int);
insert into mt185 values ('sad',1),('happy',2);
create function g185a() returns table(r text) language plpgsql as $$
#variable_conflict use_variable
declare a int := 2; b numeric(4,1) := 2.5; m mood185 := 'happy'; rec vt185b; c varchar(3) := 'x';
begin
  select * into rec from vt185b where vt185b.a = 3;
  return query select format('%s', sum(b)) from vt185b;
  return query select format('%s', pg_typeof(b)) from vt185b limit 1;
  return query select format('%s|%s', m, pg_typeof(m)) from mt185 order by mt185.a;
  return query select format('%s', count(*)) from mt185 where m > 'sad';
  <<blk>> declare a int := 3; begin
    return query select format('%s', blk.a + a) from vt185b limit 1;
    return query select format('%s', max(c)) from vt185b where vt185b.c = c;
  end;
  return query select format('%s', (select count(*) from vt185b where a = 2));
  return query select format('%s', s.x) from (select a * 10 as x from vt185b) s limit 1;
  return query with w as (select a as y from vt185b) select format('%s', y) from w limit 1;
  return query select format('%s', l.z) from vt185b, lateral (select vt185b.a + a as z) l order by 1;
  return query select format('%s', rank() over (partition by a order by b)) from vt185b;
  return query select format('%s', rec.b) from vt185b rec limit 1;
  if (select count(*) from vt185b where a = 2) = 3 then return query select 'if-yes'; end if;
  for r in select vt185b.c || a from vt185b order by vt185b.a loop return next; end loop;
end $$;
select * from g185a();
create function g185b() returns text language plpgsql as $$
#variable_conflict use_variable
declare a int := 1; c text := 'zz'; res text;
begin
  insert into vt185b(a, b, c) values (a + 10, 1, c) on conflict do nothing;
  update vt185b set c = c || '!' where vt185b.a = a returning vt185b.c into res;
  delete from vt185b where c = 'zz' and vt185b.a <> a returning vt185b.a into a;
  return res || a;
end $$;
select g185b();
select * from vt185b order by a;
create function g185c() returns int language plpgsql as $$
declare a int := 1;
begin
  return (select count(*) from vt185b where a = 1);
end $$;
select g185c();

create table vt185c(a int, b int);
insert into vt185c values (1,10),(2,20);
create function h185a() returns text language plpgsql as $$
#variable_conflict use_variable
declare a int := 2; cur refcursor; x int; res text := '';
begin
  open cur for select b from vt185c where vt185c.a = a;
  fetch cur into x; close cur; res := res || x;
  for x in select sum(a) from vt185c loop res := res || ',' || x; end loop;
  res := res || ',' || (select max(b) from vt185c where a = vt185c.a);
  return res;
end $$;
select h185a();
create table log185(n int);
create function trg185() returns trigger language plpgsql as $$
#variable_conflict use_variable
declare a int := 100;
begin
  insert into log185 select a + count(*) from vt185c;
  return new;
end $$;
create trigger t185 after insert on vt185c for each row execute function trg185();
insert into vt185c values (3, 30);
select * from log185;
create function h185b() returns int language plpgsql as $$
#variable_conflict use_variable
declare a int := 1;
begin
  return (select count(*) from vt185c where a = 1 and b = 10);
end $$;
select h185b();

create table vt185d(a int, b int);
insert into vt185d values (1,10),(2,20);
create function k185a() returns text language plpgsql as $$
#variable_conflict use_column
declare a int := 2; cur refcursor; x int; res text := '';
begin
  open cur for select sum(b) from vt185d where a = 1;
  fetch cur into x; close cur; res := res || x;
  open cur for select count(*) from vt185d group by a order by a;
  fetch cur into x; close cur; res := res || ',' || x;
  return res;
end $$;
select k185a();
create function k185b() returns text language plpgsql as $$
#variable_conflict use_variable
declare a int := 2; cur refcursor; x int; res text := '';
begin
  open cur for select sum(a) from vt185d;
  fetch cur into x; close cur; res := res || x;
  open cur for select count(*) from vt185d group by a;
  fetch cur into x; close cur; res := res || ',' || x;
  return res;
end $$;
select k185b();
create function k185c() returns text language plpgsql as $$
declare a int := 2; cur refcursor; x int;
begin
  open cur for select b from vt185d where a = 1;
  fetch cur into x; return x;
end $$;
select k185c();

DROP TABLE vt185a, vt185b, vt185c, vt185d, mt185, log185 CASCADE;
DROP TYPE mood185;
DROP ROUTINE f185a(), f185b(), g185a(), g185b(), g185c(), h185a(), trg185(), h185b(), k185a(), k185b(), k185c() CASCADE;

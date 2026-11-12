-- A PL/pgSQL cursor query with data-modifying WITH queries runs once, whole,
-- at the cursor's first read; a cursor nothing reads never writes.
create table t199(a int not null);
create function m199_0() returns text language plpgsql as $$
declare c refcursor; v int; n bigint; r text := '';
begin
  open c for with w as (insert into t199 values (1) returning a) select a, (select count(*) from t199) from w;
  insert into t199 values (7);
  fetch c into v, n;
  r := r || v || ':' || n;
  insert into t199 values (8);
  fetch c into v, n;
  r := r || ' found=' || found;
  move absolute 0 in c;
  r := r || ' ' || (select count(*) from t199);
  close c;
  open c for with w as (insert into t199 values (1) returning a) select a from w;
  fetch c into v;
  r := r || ' again ' || (select count(*) from t199);
  return r;
end $$;
select m199_0();
create function m199_3(x int) returns text language plpgsql as $$
declare c refcursor; v int;
begin
  open c for with w as (insert into t199 values (x) returning a) select a from w;
  fetch c into v;
  return 'fetched';
end $$;
select m199_3(null);
create function m199_4() returns text language plpgsql as $$
declare c refcursor; v int;
begin
  open c no scroll for with w as (insert into t199 values (1),(2) returning a) select a from w;
  fetch c into v;
  fetch prior from c into v;
  return v;
end $$;
select m199_4();
create function m199_5() returns text language plpgsql as $$
declare c refcursor; v int;
begin
  open c for with w as (insert into t199 values (1),(2) returning a) select a from w;
  fetch c into v;
  fetch prior from c into v;
  return coalesce(v::text, 'null') || found;
end $$;
select m199_5();
create function m199_6() returns setof refcursor language plpgsql as $$
declare c refcursor := 'mc199';
begin
  open c for with w as (insert into t199 values (1),(2) returning a) select a from w;
  return next c;
end $$;
begin;
select m199_6();
fetch all from mc199;
fetch backward all from mc199;
select count(*) from t199;
commit;
begin;
select m199_6();
select count(*) from t199;
commit;
select count(*) from t199;
create function m199_8() returns int language plpgsql as $$
declare c refcursor; v int; s int := 0;
begin
  open c for with w as (update t199 set a = a + 1 returning a), d as (delete from t199 where a > 5) select a from w order by a;
  loop fetch c into v; exit when not found; s := s + v; end loop;
  return s;
end $$;
select m199_8();
select a, count(*) from t199 group by a order by a;
create function m199_9() returns int language plpgsql as $$
declare c refcursor; v int; s int := 0;
begin
  open c scroll for with w as (insert into t199 select g from generate_series(1,5) g returning a) select a from w order by a desc;
  fetch absolute 3 from c into v;
  s := v;
  fetch first from c into v;
  s := s * 10 + v;
  move last in c;
  fetch prior from c into v;
  return s * 10 + v;
end $$;
select m199_9();
create function m199_10() returns int language plpgsql as $$
declare c refcursor; v int; s int := 0;
begin
  open c for execute 'with w as (insert into t199 values ($1) returning a) select a * 2 from w' using 21;
  fetch c into v;
  return v;
end $$;
select m199_10();
create function k199() returns bigint language plpgsql as $$
declare c refcursor;
begin
  open c for with w as (insert into t199 values (1) returning a) select a from w;
  return (select count(*) from t199);
end $$;
select k199(), (select count(*) from t199);
create function nf199(x int) returns int language plpgsql as $$ begin return x * 10; end $$;
create function q199() returns int language plpgsql as $$
declare c refcursor; v int;
begin
  open c for with w as (insert into t199 values (4) returning a) select nf199(a) from w;
  fetch c into v;
  return v;
end $$;
select q199();

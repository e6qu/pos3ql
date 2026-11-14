-- OLD and NEW are record variables with the table's row structure: AFTER
-- triggers, OLD, whole-record assignment, RETURNING INTO and returned rows.
create table r210(a int, b text);
create table g210(a int, b int generated always as (a * 2) stored);
create function r210_after() returns trigger language plpgsql as $$
begin
  new.a := new.a + 100;
  raise notice 'after new %', new;
  if tg_op = 'UPDATE' then
    old.b := 'changed';
    select 7 into old.a;
    raise notice 'after old %', old;
  end if;
  return null;
end $$;
create trigger r210_after after insert or update on r210 for each row execute function r210_after();
insert into r210 values (1, 'x');
update r210 set b = 'y';
select * from r210;
drop trigger r210_after on r210;
create function r210_old() returns trigger language plpgsql as $$
begin select 'from old' into old.b; old.a := old.a * 1000; return old; end $$;
create trigger r210_old before update on r210 for each row execute function r210_old();
update r210 set b = 'q';
select * from r210;
drop trigger r210_old on r210;
create function r210_null() returns trigger language plpgsql as $$
begin
  raise notice 'null % % %', new is null, new.a, old.b;
  new.a := 1;
  raise notice 'assigned %', new;
  return old;
end $$;
create trigger r210_null before delete on r210 for each row execute function r210_null();
create function r210_stmt() returns trigger language plpgsql as $$
begin raise notice 'statement % %', new is null, old; new.a := 1; return null; end $$;
create trigger r210_stmt after delete on r210 for each statement execute function r210_stmt();
delete from r210;
drop trigger r210_null on r210;
drop trigger r210_stmt on r210;
create function r210_whole() returns trigger language plpgsql as $$
declare r r210;
begin
  r := new; r.a := 50; new := r;
  select * into new from r210 limit 1;
  raise notice 'whole %', new;
  new.b := 'w';
  return new;
end $$;
create trigger r210_whole before insert on r210 for each row execute function r210_whole();
insert into r210 values (1, 'x');
insert into r210 values (7, 'x');
select * from r210;
drop trigger r210_whole on r210;
create function r210_skip() returns trigger language plpgsql as $$
begin new := null; return new; end $$;
create trigger r210_skip before insert on r210 for each row execute function r210_skip();
insert into r210 values (1, 'skipped');
drop trigger r210_skip on r210;
create function r210_row() returns trigger language plpgsql as $$
declare r r210; begin r := row(new.a * 10, 'row'); return r; end $$;
create trigger r210_row before insert on r210 for each row execute function r210_row();
insert into r210 values (3, 'x');
drop trigger r210_row on r210;
create function r210_ret() returns trigger language plpgsql as $$
begin return row(5, 6); end $$;
create trigger r210_ret before insert on r210 for each row execute function r210_ret();
insert into r210 values (1, 'x');
drop trigger r210_ret on r210;
create function r210_scalar() returns trigger language plpgsql as $$
begin return 1; end $$;
create trigger r210_scalar before insert on r210 for each row execute function r210_scalar();
insert into r210 values (1, 'x');
drop trigger r210_scalar on r210;
create function r210_bad() returns trigger language plpgsql as $$
begin new := 5; return new; end $$;
create trigger r210_bad before insert on r210 for each row execute function r210_bad();
insert into r210 values (1, 'x');
drop trigger r210_bad on r210;
create function r210_field() returns trigger language plpgsql as $$
begin new.a := 1; old.zz := 5; return new; end $$;
create trigger r210_field before insert on r210 for each row execute function r210_field();
insert into r210 values (1, 'x');
drop trigger r210_field on r210;
create function r210_read() returns trigger language plpgsql as $$
begin raise notice '%', new.zz; return new; end $$;
create trigger r210_read before insert on r210 for each row execute function r210_read();
insert into r210 values (1, 'x');
drop trigger r210_read on r210;
create function r210_returning() returns trigger language plpgsql as $$
begin
  insert into r210 values (new.a + 1, 'ret') returning a * 10, b into new.a, old.b;
  raise notice '% %', new, old;
  return new;
end $$;
create trigger r210_returning before update on r210 for each row execute function r210_returning();
update r210 set b = 'upd' where a = 30;
select * from r210 order by a;
create function g210f() returns trigger language plpgsql as $$
begin new.b := 5; raise notice '%', new; return new; end $$;
create trigger g210_before before insert on g210 for each row execute function g210f();
insert into g210 values (1);
create trigger g210_after after insert on g210 for each row execute function g210f();
insert into g210 values (2);
select * from g210 order by a;
drop table r210, g210;
drop function r210_after(), r210_old(), r210_null(), r210_stmt(), r210_whole(), r210_skip(),
  r210_row(), r210_ret(), r210_scalar(), r210_bad(), r210_field(), r210_read(), r210_returning(),
  g210f();

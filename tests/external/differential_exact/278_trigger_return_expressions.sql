-- A trigger function may RETURN any row-valued expression: NULL skips the
-- row, a row of the triggering relation's column types by position replaces
-- it in a BEFORE INSERT or UPDATE trigger, and other rows fail at function
-- exit. A non-row value fails at the RETURN.
CREATE SCHEMA trigger_return_expressions;
SET search_path = trigger_return_expressions, public;
create table t(a int, b text);
create function f() returns trigger language plpgsql as $$
begin raise notice '% %', tg_op, coalesce(new, old); return coalesce(new, old); end $$;
create trigger tb before insert or update or delete on t for each row execute function f();
insert into t values (1, 'x');
update t set b = 'y';
delete from t;
create or replace function f() returns trigger language plpgsql as $$
begin if tg_op = 'DELETE' then return null; end if; return row(new.a + 1, upper(new.b)); end $$;
insert into t values (2, 'z') returning *;
delete from t returning *;
create or replace function f() returns trigger language plpgsql as $$
begin return case when new.a > 5 then new else null end; end $$;
insert into t values (3, 'q'), (7, 'w') returning *;
create or replace function f() returns trigger language plpgsql as $$
begin return (select x from t x limit 1); end $$;
insert into t values (9, 'k') returning *;
create or replace function f() returns trigger language plpgsql as $$
begin return 1; end $$;
insert into t values (10, 'k') returning *;
create or replace function f() returns trigger language plpgsql as $$
begin return row(1); end $$;
insert into t values (11, 'k') returning *;
create or replace function f() returns trigger language plpgsql as $$
begin return row(1, 2, 3); end $$;
insert into t values (12, 'k') returning *;
create or replace function f() returns trigger language plpgsql as $$
begin return row('5', 6); end $$;
insert into t values (13, 'k') returning *;
create or replace function f() returns trigger language plpgsql as $$
declare r record; begin r := row(42, 'o'::text); return r; end $$;
insert into t values (14, 'k') returning *;
create or replace function g() returns trigger language plpgsql as $$
begin return coalesce(new, old); end $$;
create trigger ta after insert on t for each row execute function g();
insert into t values (15, 'k') returning *;
create or replace function g() returns trigger language plpgsql as $$
begin return row(1); end $$;
insert into t values (16, 'k') returning *;
create or replace function g() returns trigger language plpgsql as $$
begin return 5; end $$;
insert into t values (17, 'k') returning *;
drop trigger ta on t;
create or replace function s() returns trigger language plpgsql as $$
begin return row(1); end $$;
create trigger ts before insert on t for each statement execute function s();
insert into t values (18, 'k') returning *;
create or replace function s() returns trigger language plpgsql as $$
begin return 2; end $$;
insert into t values (19, 'k') returning *;
select * from t;
create domain short_text as varchar(10);
create type mood as enum ('sad', 'happy');
create table typed(id int, label short_text, feeling mood);
create function typed_row() returns trigger language plpgsql as $$
declare r record;
begin
  select new.id * 10, new.label, 'happy'::mood into r;
  if new.id = 2 then return (select x from typed x where false); end if;
  if new.id = 3 then return row(new.id, new.label::text, new.feeling); end if;
  return r;
end $$;
create trigger typed_row before insert or update on typed for each row execute function typed_row();
insert into typed values (1, 'one', 'sad') returning *;
insert into typed values (2, 'two', 'sad') returning *;
insert into typed values (3, 'three', 'sad') returning *;
update typed set feeling = 'sad' returning *;
create view typed_view as select id, label from typed;
create function typed_instead() returns trigger language plpgsql as $$
begin insert into typed values (new.id, new.label, 'sad'); return coalesce(new, old); end $$;
create trigger typed_instead instead of insert or delete on typed_view for each row execute function typed_instead();
insert into typed_view values (4, 'four') returning *;
DROP SCHEMA trigger_return_expressions CASCADE;

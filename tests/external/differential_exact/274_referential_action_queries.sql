-- ON DELETE and ON UPDATE CASCADE, SET NULL and SET DEFAULT run the query
-- PostgreSQL's ri_triggers.c builds (DELETE FROM ONLY child ... or UPDATE
-- ONLY child SET ...): the child's rules rewrite it, its BEFORE triggers fire
-- (BEFORE STATEMENT once per table and operation in the statement), and its
-- AFTER events join the end of the statement's queue, where AFTER STATEMENT
-- fires once per table and operation with every changed row.
CREATE SCHEMA referential_action_queries;
SET search_path = referential_action_queries, public;
create function trace() returns trigger language plpgsql as $$
declare n int;
begin
  if tg_level = 'STATEMENT' then
    if tg_when = 'AFTER' and tg_op = 'DELETE' then
      execute 'select count(*) from old_rows' into n;
    elsif tg_when = 'AFTER' and tg_op = 'UPDATE' then
      execute 'select count(*) from new_rows' into n;
    end if;
    raise notice '% % % % rows=%', tg_table_name, tg_when, tg_level, tg_op, n;
    return null;
  end if;
  raise notice '% % % % % -> %', tg_table_name, tg_when, tg_level, tg_op, old, new;
  if tg_op = 'DELETE' then return old; end if;
  return new;
end $$;
create table p(id int primary key);
create table c(id int, pid int references p on delete cascade on update set null);
create trigger c_bs before delete or update on c for each statement execute function trace();
create trigger c_br before delete or update on c for each row execute function trace();
create trigger c_ar after delete or update on c for each row execute function trace();
create trigger c_as after delete on c referencing old table as old_rows for each statement execute function trace();
create trigger c_asu after update on c referencing new table as new_rows for each statement execute function trace();
create trigger p_as after delete on p referencing old table as old_rows for each statement execute function trace();
create trigger p_ar after delete on p for each row execute function trace();
insert into p values (1), (2), (3), (4);
insert into c values (10, 1), (11, 1), (20, 2), (30, 3), (40, 4);
delete from p where id <= 2;
update p set id = id + 10 where id >= 3;
select * from c order by id;
-- A cascade that matches nothing still fires the statement triggers.
delete from p where id = 13;
-- A self-referencing cascade: one BEFORE and one AFTER STATEMENT trigger,
-- whose transition table holds the outer and the cascaded rows.
create table tree(k int primary key, parent int references tree on delete cascade);
create trigger t_bs before delete on tree for each statement execute function trace();
create trigger t_as after delete on tree referencing old table as old_rows for each statement execute function trace();
insert into tree values (1, null), (2, 1), (3, 2), (4, null);
delete from tree where k = 1;
select * from tree;
-- A BEFORE ROW trigger can skip the cascaded change or alter it.
create table q(id int primary key);
create table qc(id int, qid int references q on delete cascade on update cascade);
create function keep_odd() returns trigger language plpgsql as $$
begin
  if tg_op = 'DELETE' then
    if old.id % 2 = 1 then return null; end if;
    return old;
  end if;
  new.id := new.id * 10;
  return new;
end $$;
create trigger qc_keep before delete or update on qc for each row execute function keep_odd();
insert into q values (1), (2);
insert into qc values (1, 1), (2, 1), (3, 2);
update q set id = 5 where id = 2;
select * from qc order by id;
delete from q where id = 1;
select * from qc order by id;
-- Errors inside the action name its query as context.
create table e(id int primary key);
create table ec(id int, eid int references e on delete cascade);
create function boom() returns trigger language plpgsql as $$
begin raise exception 'boom in %', tg_table_name; end $$;
create trigger ec_boom before delete on ec for each row execute function boom();
insert into e values (1);
insert into ec values (1, 1);
delete from e;
create table e2(a int, b text, primary key (a, b));
create table e2c(a bigint, b varchar, foreign key (a, b) references e2 on update cascade on delete set default);
create trigger e2c_boom before update on e2c for each row execute function boom();
insert into e2 values (1, 'x');
insert into e2c values (1, 'x');
update e2 set a = 2;
delete from e2;
create table "Quoted ""Parent"""(id int primary key);
create table "Quoted ""Child"""("Key ""Column""" int references "Quoted ""Parent""" on delete set null);
create trigger quoted_boom before update on "Quoted ""Child""" for each row execute function boom();
insert into "Quoted ""Parent""" values (1);
insert into "Quoted ""Child""" values (1);
delete from "Quoted ""Parent""";
-- An AFTER trigger of the cascaded change fires with the statement's queue,
-- outside the action query.
drop trigger ec_boom on ec;
create trigger ec_after_boom after delete on ec for each row execute function boom();
delete from e;
-- Rules rewrite the action. When no query of the action's command remains
-- to report its result, PostgreSQL fails it.
create table r(id int primary key);
insert into r values (1), (2), (3), (4), (5), (6);
create table log(id int);
create table r1(id int, rid int references r on delete cascade);
create rule r1_nothing as on delete to r1 do instead nothing;
insert into r1 values (1, 1);
delete from r where id = 1;
drop table r1;
create table r2(id int, rid int references r on delete cascade);
create rule r2_other as on delete to r2 do instead delete from log where id = old.id;
insert into r2 values (2, 2);
insert into log values (2);
delete from r where id = 2;
select (select count(*) from r2) as r2, (select count(*) from log) as log;
drop table r2;
create table r3(id int, rid int references r on delete set null);
create rule r3_insert as on update to r3 do instead insert into log values (old.id);
insert into r3 values (3, 3);
delete from r where id = 3;
drop table r3;
create table r4(id int, rid int references r on delete cascade);
create rule r4_also as on delete to r4 do also insert into log values (old.id);
insert into r4 values (4, 4);
delete from r where id = 4;
select (select count(*) from r4) as r4, (select count(*) from log where id = 4) as log;
drop table r4;
create table r5(id int, rid int references r on delete cascade);
create rule r5_some as on delete to r5 where old.id > 100 do instead nothing;
insert into r5 values (5, 5);
delete from r where id = 5;
select count(*) from r5;
drop table r5;
-- The action runs as the child table's owner, which FORCE ROW LEVEL
-- SECURITY does not restrict there.
create role referential_action_owner;
grant usage on schema referential_action_queries to referential_action_owner;
create table o(id int primary key);
create table oc(id int, oid int references o on delete cascade);
create function who() returns trigger language plpgsql as $$
begin raise notice '% sees %', tg_op, current_user; return old; end $$;
create trigger oc_who before delete on oc for each row execute function who();
insert into o values (1);
insert into oc values (1, 1);
alter table oc owner to referential_action_owner;
alter table oc enable row level security;
alter table oc force row level security;
create policy oc_none on oc using (false);
delete from o;
select count(*) from oc;
drop table oc;
drop owned by referential_action_owner;
drop role referential_action_owner;
-- A data-modifying WITH sub-statement's AFTER triggers fire after the main
-- statement, so they see its rows.
create table w(id int);
create table wlog(id int);
create function count_log() returns trigger language plpgsql as $$
begin raise notice 'wlog has % rows', (select count(*) from wlog); return null; end $$;
create trigger w_after after insert on w for each row execute function count_log();
with x as (insert into w values (1)) insert into wlog values (1);
with x as (insert into w values (2) returning id) select count(*) from x;
DROP SCHEMA referential_action_queries CASCADE;

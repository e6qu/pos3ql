-- session_replication_role selects which triggers and rules fire: replica
-- fires only those enabled REPLICA or ALWAYS, origin and local those enabled
-- ORIGIN or ALWAYS. Foreign key triggers are ORIGIN, so replica skips their
-- checks and actions; only superusers or roles granted SET may change it.
CREATE SCHEMA session_replication_role;
SET search_path = session_replication_role, public;
show session_replication_role;
select name, setting, category, short_desc, context, vartype, enumvals, boot_val, reset_val
  from pg_settings where name = 'session_replication_role';
create table p(id int primary key);
create table c(id int, pid int references p on delete cascade);
create table log(msg text);
create function note() returns trigger language plpgsql as $$
begin insert into log values (tg_name || ' ' || tg_op); return new; end $$;
create trigger t_origin before insert on c for each row execute function note();
create trigger t_replica before insert on c for each row execute function note();
create trigger t_always before insert on c for each row execute function note();
create trigger t_disabled before insert on c for each row execute function note();
create trigger s_origin after insert on c for each statement execute function note();
create trigger s_replica after insert on c for each statement execute function note();
alter table c enable replica trigger t_replica;
alter table c enable always trigger t_always;
alter table c disable trigger t_disabled;
alter table c enable replica trigger s_replica;
select tgname, tgenabled from pg_trigger
  where tgrelid = 'c'::regclass and tgname !~ '^RI_' order by tgname;
insert into p values (1);
insert into c values (1, 1);
set session_replication_role = replica;
show session_replication_role;
-- An orphan is accepted and a parent delete cascades nothing.
insert into c values (2, 99);
delete from p where id = 1;
select * from c order by id;
set session_replication_role = local;
insert into c values (3, 1);
reset session_replication_role;
select * from log;
select current_setting('session_replication_role');
begin;
set local session_replication_role = replica;
insert into c values (5, 97);
rollback;
show session_replication_role;
-- Rules follow the same firing modes.
create table r(a int);
create table rlog(a int);
create rule r_ins as on insert to r do also insert into rlog values (new.a);
create rule r_rep as on insert to r do also insert into rlog values (new.a + 100);
alter table r enable replica rule r_rep;
create rule r_alw as on insert to r do also insert into rlog values (new.a + 1000);
alter table r enable always rule r_alw;
insert into r values (1);
set session_replication_role = replica;
insert into r values (2);
select * from rlog order by a;
select ev_enabled, rulename from pg_rewrite where ev_class = 'r'::regclass order by rulename;
reset session_replication_role;
-- A view's INSTEAD OF trigger is ORIGIN: a replica skips it without an
-- error, and a view keeps automatic updates for events it has no trigger for.
create table vt(a int);
create view vv as select a from vt;
create function vf() returns trigger language plpgsql as $$
begin insert into vt values (new.a); return new; end $$;
create trigger vtr instead of insert on vv for each row execute function vf();
insert into vv values (4);
update vv set a = 5 returning *;
set session_replication_role = replica;
insert into vv values (6);
reset session_replication_role;
delete from vv where a = 5 returning *;
select * from vt;
set session_replication_role = foo;
create role session_replication_role_user;
grant usage on schema session_replication_role to session_replication_role_user;
set role session_replication_role_user;
set session_replication_role = replica;
select set_config('session_replication_role', 'replica', false);
reset session_replication_role;
reset role;
grant set on parameter session_replication_role to session_replication_role_user;
set role session_replication_role_user;
set session_replication_role = replica;
show session_replication_role;
select set_config('session_replication_role', 'origin', false);
reset role;
reset session_replication_role;
revoke set on parameter session_replication_role from session_replication_role_user;
drop owned by session_replication_role_user;
drop role session_replication_role_user;
DROP SCHEMA session_replication_role CASCADE;

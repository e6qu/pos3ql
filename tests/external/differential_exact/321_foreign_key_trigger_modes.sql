-- A foreign key's RI triggers have their own firing modes: DISABLE TRIGGER
-- ALL on the referencing table stops its checks, on the referenced table its
-- actions; USER leaves them alone. A replica fires only REPLICA or ALWAYS ones.
CREATE SCHEMA foreign_key_trigger_modes;
SET search_path = foreign_key_trigger_modes, public;
create table p(id int primary key);
create table c(id int, pid int references p on delete cascade);
create function n() returns trigger language plpgsql as $$begin return new; end$$;
create trigger u after insert on c for each row execute function n();
alter table c disable trigger all;
alter table c disable trigger "RI_ConstraintTrigger_c_0";
select tgname ~ '^RI_' as ri, tgenabled from pg_trigger where tgrelid in ('c'::regclass, 'p'::regclass) order by tgrelid::regclass::text, 1, 2;
insert into c values (1, 5);
alter table c enable trigger all;
alter table c disable trigger user;
select tgname ~ '^RI_' as ri, tgenabled from pg_trigger where tgrelid='c'::regclass order by 1, 2;
insert into c values (2, 6);
insert into c values (3, 7);
alter table c enable trigger all;
insert into p values (1);
insert into c values (4, 1);
alter table p disable trigger all;
select tgname ~ '^RI_' as ri, tgenabled from pg_trigger where tgrelid='p'::regclass order by 1, 2;
delete from p where id = 1;
select * from c order by id;
alter table p enable trigger all;
insert into p values (2);
insert into c values (5, 2);
select * from c order by id;
DO $$DECLARE t name; BEGIN
  FOR t IN SELECT tgname FROM pg_trigger WHERE tgrelid = 'c'::regclass AND tgname LIKE 'RI_%' LOOP
    EXECUTE format('ALTER TABLE c ENABLE ALWAYS TRIGGER %I', t);
  END LOOP;
  FOR t IN SELECT tgname FROM pg_trigger WHERE tgrelid = 'p'::regclass AND tgname LIKE 'RI_%' LOOP
    EXECUTE format('ALTER TABLE p ENABLE REPLICA TRIGGER %I', t);
  END LOOP;
END$$;
select c.relname, t.tgenabled from pg_trigger t join pg_class c on c.oid = t.tgrelid
  where t.tgname like 'RI_%' order by 1, 2;
set session_replication_role = replica;
insert into c values (6, 99);
delete from p;
reset session_replication_role;
select * from c order by id;
-- Partitions keep their own check triggers; ONLY stops at the parent.
create table pp(id int primary key);
create table pc(id int, pid int references pp) partition by range(id);
create table pc1 partition of pc for values from (0) to (10);
alter table pc1 disable trigger all;
select tgrelid::regclass, tgenabled, count(*) from pg_trigger where tgname like 'RI_%' group by 1,2 order by 1,2;
insert into pc values (1, 5);
alter table pc disable trigger all;
select tgrelid::regclass, tgenabled, count(*) from pg_trigger where tgname like 'RI_%' group by 1,2 order by 1,2;
insert into pc values (2, 6);
alter table pc enable trigger all;
alter table only pc disable trigger all;
select tgrelid::regclass, tgenabled, count(*) from pg_trigger where tgname like 'RI_%' group by 1,2 order by 1,2;
insert into pc values (3, 7);
alter table pc enable trigger all;
insert into pp values (1);
insert into pc values (4, 1);
alter table pp disable trigger all;
delete from pp;
select * from pc order by id;
DROP SCHEMA foreign_key_trigger_modes CASCADE;

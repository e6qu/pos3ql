-- Foreign key checks run as AFTER ROW events at the statement's end, among
-- the row's AFTER triggers in trigger-name order (RI_ConstraintTrigger_*).
CREATE SCHEMA foreign_key_trigger_order;
SET search_path = foreign_key_trigger_order, public;
create table tree(id int primary key, parent int references tree);
insert into tree values (2, 1), (1, null);
select * from tree order by 1;
insert into tree values (3, 4), (5, null);
delete from tree where id = 1;
delete from tree where id in (1, 2);
create table p(id int primary key);
create table c(id int, pid int references p);
create function addp() returns trigger language plpgsql as $$
begin insert into p values (new.pid) on conflict do nothing; return null; end $$;
create trigger "A_addp" after insert on c for each row execute function addp();
insert into c values (1, 5);
create trigger "Z_addp" after insert on c for each row execute function addp();
drop trigger "A_addp" on c;
insert into c values (2, 6);
create table cd(id int, pid int references p deferrable initially immediate);
create trigger "A_addp" after insert on cd for each row execute function addp();
insert into cd values (1, 7);
select * from p order by 1;
-- NO ACTION passes when the old key exists again at the statement's end;
-- RESTRICT does not, and words its detail without "still".
create table pk(id int primary key);
insert into pk values (1), (2), (3), (4);
create table cna(id int, pid int references pk);
create table cr(id int, pid int references pk on delete restrict on update restrict);
create table cdf(id int, pid int references pk deferrable initially deferred);
insert into cna values (1, 1);
insert into cr values (1, 2);
update pk set id = case id when 1 then 5 when 5 then 1 end where id in (1, 5);
insert into pk values (5);
update pk set id = case id when 1 then 5 when 5 then 1 end where id in (1, 5);
update pk set id = case id when 2 then 6 when 6 then 2 end where id in (2);
delete from pk where id = 2;
delete from pk where id = 1;
-- Deferred checks run at commit and SET CONSTRAINTS; rollback re-arms them.
begin;
insert into cdf values (1, 3);
delete from pk where id = 3;
commit;
begin;
insert into cdf values (1, 3);
delete from pk where id = 3;
insert into pk values (3);
commit;
select * from cdf;
begin;
insert into cdf values (2, 77);
savepoint s;
set constraints all immediate;
rollback to s;
delete from cdf where pid = 77;
commit;
begin;
insert into cdf values (3, 78);
set constraints cdf_pid_fkey immediate;
rollback;
-- A nested statement fires its checks at its own end.
create function nested_check() returns text language plpgsql as $$
begin
  begin
    insert into cna values (10, 1000);
  exception when foreign_key_violation then
    return 'caught ' || sqlerrm;
  end;
  return 'none';
end $$;
select nested_check();
create table pc(id int primary key);
create table cc(id int, pid int references pc on delete cascade on update cascade);
insert into pc values (1), (2);
insert into cc values (1, 1), (2, 2);
update pc set id = 10 where id = 1;
delete from pc where id = 2;
select * from cc order by id;
-- More queued events than one in-memory page.
create table big(id int primary key);
insert into big select generate_series(1, 2000);
create table bigc(id int, pid int references big);
insert into bigc select g, g from generate_series(1, 2000) g;
insert into bigc select g, g + 1 from generate_series(1998, 2000) g;
select count(*) from bigc;
delete from big where id > 1990;
RESET search_path;
DROP SCHEMA foreign_key_trigger_order CASCADE;

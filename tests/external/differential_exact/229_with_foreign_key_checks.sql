-- Foreign key checks queued by data-modifying WITH sub-statements run at the
-- end of the whole statement, after its main statement, and see every row
-- the statement wrote.
CREATE SCHEMA with_foreign_key_checks;
SET search_path = with_foreign_key_checks, public;
create table p(id int primary key);
create table c(pid int references p);
create table q(id int);
insert into p values (1), (2);
insert into c values (1);
-- NO ACTION passes: the main statement provides the deleted key again.
with d as (delete from p where id = 1 returning id) insert into p select id from d;
with d as (delete from p where id = 1 returning id) select * from d;
with d as (delete from p where id = 2 returning id) select * from d;
-- A referencing row inserted by a sub-statement is checked after the main
-- statement inserted its parent, and checked when the main statement reads.
with i as (insert into c values (3)) insert into p values (3);
with i as (insert into c values (4)) select 1;
with i as (insert into c values (4)) insert into q values (4);
with i as (insert into c values (5) returning pid) insert into p select pid from i;
with i as (insert into c values (6) returning pid), j as (insert into p values (6))
select * from i;
begin;
with d as (delete from p where id = 1 returning id), e as (insert into p select id from d)
select * from d;
rollback;
-- RESTRICT is not satisfied by the main statement's new row.
create table cr(pid int references p on delete restrict);
insert into cr values (3);
with d as (delete from p where id = 3 returning id) insert into p select id from d;
-- A deferred check still waits for commit.
create table cd(pid int references p deferrable initially deferred);
begin;
with i as (insert into cd values (7)) insert into p values (8);
insert into p values (7);
commit;
select * from p order by 1;
select * from c order by 1;
select * from cd order by 1;
select count(*) from q;

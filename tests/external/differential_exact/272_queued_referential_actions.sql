-- ON DELETE and ON UPDATE CASCADE, SET NULL and SET DEFAULT run at the end of
-- the statement that changed the referenced rows, among that row's AFTER
-- events (PostgreSQL's RI_ConstraintTrigger_a_* triggers sort before
-- lower-case trigger names), so they see every row the statement changed.
CREATE SCHEMA queued_referential_actions;
SET search_path = queued_referential_actions, public;
create table tree(k int primary key, parent int references tree on update cascade);
insert into tree values (1, null), (2, 1), (3, 2), (4, 3);
update tree set k = k + 10;
select * from tree order by k;
create table chain(k int primary key, parent int references chain on delete cascade);
insert into chain values (1, null), (2, 1), (3, 2), (4, 3), (5, null);
delete from chain where k in (1, 3);
select * from chain order by k;
delete from chain;
select count(*) from chain;
-- Swapping two referenced keys in one statement: each queued action reads
-- the rows the earlier one rewrote.
create table p(id int primary key);
create table c(id int primary key, pid int references p on update cascade);
insert into p values (1), (2);
insert into c values (10, 1), (20, 2);
update p set id = case id when 1 then 2 else 1 end;
select * from c order by id;
-- A cascade reaches grandchildren through their own actions.
create table g(id int primary key, cid int references c on update cascade on delete set null);
insert into g values (100, 10), (200, 20);
update c set id = id + 1;
select * from g order by id;
delete from c where id = 11;
select * from g order by id;
-- SET DEFAULT whose default is the deleted key fails as NO ACTION would.
create table pk(a int, b int, primary key (a, b));
insert into pk values (1, 1), (0, 0);
create table fk(x int default 0, y int default 0, foreign key (x, y) references pk on delete set default);
insert into fk values (1, 1);
delete from pk where a = 1;
select * from fk;
delete from pk where a = 0;
select * from fk;
-- The referenced table's AFTER trigger fires after the action of its row.
create table orders(id int primary key);
create table lines(id int, order_id int references orders on delete cascade);
create function count_lines() returns trigger language plpgsql as $$
begin
  raise notice 'order % leaves % lines', old.id, (select count(*) from lines);
  return old;
end $$;
create trigger orders_after after delete on orders for each row execute function count_lines();
insert into orders values (1), (2);
insert into lines values (1, 1), (2, 1), (3, 2);
delete from orders;
select count(*) from lines;
-- A cascaded key that does not fit fails when the action runs.
create table wide(id bigint primary key);
create table narrow(k smallint references wide on update cascade);
insert into wide values (1);
insert into narrow values (1);
update wide set id = 100000;
select * from narrow;
-- Deferrable keys still run their actions at the statement's end.
create table dp(id int primary key);
create table dc(pid int references dp on delete cascade deferrable initially deferred);
insert into dp values (1);
insert into dc values (1);
begin;
delete from dp;
select count(*) from dc;
commit;
DROP SCHEMA queued_referential_actions CASCADE;

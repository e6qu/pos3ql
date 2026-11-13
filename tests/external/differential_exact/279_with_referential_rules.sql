-- A data-modifying WITH sub-statement's referential actions run with the
-- full executor at the end of the statement, so rules on the referencing
-- table fire whether the main statement is a SELECT or a write.
CREATE SCHEMA with_referential_rules;
SET search_path = with_referential_rules, public;
create table p(id int primary key);
create table c(id int, pid int references p on delete cascade on update cascade);
create table clog(id int, op text);
create rule cr as on delete to c do also insert into clog values (old.id, 'd');
create rule cu as on update to c do also insert into clog values (new.id, 'u');
insert into p values (1), (2), (3);
insert into c values (1, 1), (2, 2), (3, 3);
with d as (delete from p where id = 1 returning id) select * from d;
with u as (update p set id = 20 where id = 2 returning id) select * from u;
with d as (delete from p where id = 3 returning id) select count(*) from d;
select * from c order by id;
select * from clog order by id;
create table c2(id int, pid int references p on delete cascade);
create table clog2(id int, op text);
create rule cr2 as on delete to c2 do also insert into clog2 values (old.id, 'd');
insert into p values (1), (2);
insert into c2 values (1, 1), (2, 2);
with d as (delete from p where id = 1 returning id) insert into clog2 select id, 'x' from d;
select * from clog2 order by id, op;
DROP SCHEMA with_referential_rules CASCADE;

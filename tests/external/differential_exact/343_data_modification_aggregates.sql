-- Aggregates of a data-modifying statement's own level are rejected in its
-- VALUES, WHERE, RETURNING and SET clauses, also through subqueries, and a
-- view's correlated subquery records the columns it reads.
create table tq (v int, w int);
create table uq (x int);
insert into tq values (1, 2);
update tq set v = 1 where max(v) > 0;
update tq set v = 1 where (select max(tq.v) from uq) > 0;
update tq set v = (select max(tq.v));
update tq set v = 1 returning max(v);
update tq set v = (select max(x) from uq) where (select count(*) from uq) = 0 returning (select sum(x) from uq);
delete from tq where (select max(tq.v) from (values (1)) x) > 0;
delete from tq using uq where sum(uq.x) > 0;
delete from tq where v = 1 returning (select max(tq.w));
insert into tq values (max(1), 2);
insert into tq select max(v), 1 from tq;
create view cv as select v, (select x from uq where uq.x = tq.v) from tq;
select * from cv;
select d.refobjsubid from pg_depend d join pg_rewrite r on r.oid = d.objid
where r.ev_class = 'cv'::regclass and d.refobjid = 'tq'::regclass order by 1;
alter table tq drop column v;
drop view cv;
-- An aggregate's scalar subquery argument has the subquery's type.
insert into uq values (3);
select sum((select x from uq limit 1)) from tq;
select w, sum((select x from uq limit 1)) from tq group by w order by 1;
select sum((select x from uq limit 1)) over () from tq;
drop table tq, uq;

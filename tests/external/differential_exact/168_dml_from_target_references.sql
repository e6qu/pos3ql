-- FROM, USING and MERGE source items cannot refer to the statement's target:
-- function sources and LATERAL subqueries fail with 42P10, other items with
-- 42P01, before any row is read.
CREATE SCHEMA target_references;
SET search_path = target_references, public;
create table lt(id int primary key, v int); create table ls(id int, w int);
insert into lt values (1,1); insert into ls values (1,1);
update lt set v = 1 from (select lt.v as x) s;
update lt set v = 1 from generate_series(1, lt.v) g;
update lt set v = 1 from generate_series(1, v) g;
delete from lt using lateral (select lt.v as x) s;
update lt set v = 1 from ls join lateral (select lt.v) s on true;
update lt set v = 1 from ls join ls l2 on l2.id = lt.id;
merge into lt using lateral (select lt.v as x) s on true when matched then do nothing;
merge into lt using (select lt.v as x) s on true when matched then do nothing;
merge into lt using generate_series(1, lt.v) g on true when matched then do nothing;
update lt x set v = 1 from lateral (select x.v as y) s;
update lt x set v = 1 from (select x.v as y) s;
update lt set v = 1 from ls where ls.id = (select lt.id);
update lt set v = 1 from ls lt2, lateral (select lt2.w) s where lt2.id = lt.id;
update lt set v = 1 from (select l.v from lt l) s where s.v = lt.v;
update lt set v = 1 from generate_series(1, 2) g, generate_series(1, g) h where lt.id = h;
update lt set v = 1 from lateral (select v as y) s;
update lt set v = 1 from ls, lateral (select v as y) s;
update lt set v = 1 from ls, lateral (select w as y) s where ls.id = lt.id;
update lt set v = 2 from lateral (select w as y from ls) s where s.y = lt.id;
update lt set v = 1 from ls, generate_series(1, w) g where ls.id = lt.id;
select * from lt;

-- A FROM, USING or MERGE source subquery that is not LATERAL cannot see the
-- target's columns: an unqualified one fails with 42703 and PostgreSQL's
-- detail naming the target, even when the target has no rows.
CREATE SCHEMA hidden_target_columns;
SET search_path = hidden_target_columns, public;
create table lt(id int primary key, v int); create table ls(id int, w int);
update lt set v = 1 from (select v as x) s;
update lt set v = 1 from (select w from ls where w = v) s;
update lt set v = 1 from (select w from ls where w = id) s;
delete from lt using (select w from ls where w = v) s;
update lt x set v = 1 from (select v from ls) s;
merge into lt using (select v as x) s on true when matched then do nothing;
insert into lt values (1, 1); insert into ls values (1, 1);
update lt set v = 2 from (select w from ls where w = id) s;
update lt set v = 2 from (select id, w from ls) s where s.w = v;
select * from lt;

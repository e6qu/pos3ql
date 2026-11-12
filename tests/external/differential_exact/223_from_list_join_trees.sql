-- A comma-separated FROM item starts a new join tree: a later USING or
-- NATURAL join merges only with the items since the last comma, and its
-- merged columns lead that tree's columns, not the whole FROM list's.
CREATE SCHEMA from_list_join_trees;
SET search_path = from_list_join_trees, public;
create table x(z int, id int); insert into x values (100, 1);
create table a(id int, s text); insert into a values (1, 'a1'), (2, 'a2');
create table b(id int, z int); insert into b values (1, 7), (2, 8);
select * from x, a join b using (id) order by a.id;
select * from x, a natural join b order by a.id;
select * from a join b using (id), x order by a.id;
select * from x, a natural join b, x x2 natural join a a2 order by a.id;
select x.*, j.* from x, a join b using (id) as j order by j.id;
select * from x, a join b using (z);
select * from x, a left join b using (id) order by a.id;

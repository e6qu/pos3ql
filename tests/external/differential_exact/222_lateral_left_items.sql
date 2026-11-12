-- A LATERAL body reads only the FROM items to its left: never its own
-- columns or later items'. The right side of a RIGHT or FULL join reads none
-- of them, and UPDATE/DELETE bodies cannot read the target.
CREATE SCHEMA lateral_left_items;
SET search_path = lateral_left_items, public;
create table lt(id int, v int); insert into lt values (1,10),(2,20);
create table lu(id int, v int); insert into lu values (1,100);
create table lw(w int); insert into lw values (5);
select * from lt, lateral (select v) s order by 1;
select * from lt a, lateral (select v) s, lateral (select s.v + 1) t order by 1;
select * from lt a left join lateral (select v where v > 10) s on true order by 1;
select * from lt a, lateral (select v) s, lu order by 1;
select * from lt a join lu b using (id), lateral (select v) s;
select * from lt a join lu b using (v), lateral (select v) s;
select * from lt a join lu b on a.id = b.id, lateral (select v) s;
select * from lt a natural join lu b, lateral (select v, id) s;
select * from lt a join lu b using (id) as j, lateral (select j.id) s;
select * from lt a, lateral (select s.v) s;
select * from lt, unnest(array[v]) as u(v) order by 1;
select * from lt, unnest(array[v, v + 1]) as v order by 1, 3;
select * from unnest(array[1]) u;
select * from lt, generate_series(1, id) as g(id) order by 1, 3;
select (select count(*) from lu x, lateral (select v) s) from lt order by 1;
select * from lt a right join lateral (select 1 as one) s on true order by 1;
select * from lt a full join lateral (select v) s on true;
select * from lt a right join lateral (select a.id) s on true;
select * from lt a full join lateral (select * from lu where lu.id = a.id) s on true;
select * from lt a right join lateral (select * from lu) s on a.id = s.id order by 1;
update lt set v = 1 from lu x, generate_series(1, id) g;
update lt set v = 1 from lu x, lateral (select v) s;
delete from lt using lu x, lateral (select v) s where lt.id = x.id;
update lt set v = v + s.w from lw, lateral (select w) s where lt.id = 1 returning lt.id, lt.v;
delete from lt using lw, lateral (select w) s where false;

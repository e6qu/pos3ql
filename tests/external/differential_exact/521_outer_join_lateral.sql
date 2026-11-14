-- A LATERAL item on the right of a RIGHT or FULL join may read FROM items
-- outside that join, never the join's other side.
create table a211 (x int); create table b211 (y int);
insert into a211 values (1), (2); insert into b211 values (10), (20);
select * from a211 right join lateral (select 1 as k) s on true order by 1, 2;
select * from b211, a211 right join lateral (select b211.y) s on a211.x = 1 order by 1, 2, 3;
select * from b211, a211 full join lateral (select b211.y + 1 as z) s on a211.x * 10 = s.z - 1 order by 1, 2, 3;
select * from a211 right join lateral (select a211.x) s on true;
select * from a211 full join lateral (select a211.x) s on true;
select * from b211, a211 right join lateral (select y from b211 b2 where b2.y > b211.y) s on true order by 1, 2, 3;
select * from b211 cross join lateral (select * from a211 right join lateral (select b211.y as w) s on a211.x = 2) q order by 1, 2, 3;
select * from a211 right join generate_series(1, 3) g on g = a211.x order by 2;
select * from a211 full join lateral generate_series(2, 3) g on g = a211.x order by 1, 2;
select * from b211, a211 right join lateral generate_series(1, b211.y / 10) g on g = a211.x order by 1, 2, 3;
select * from a211 right join lateral generate_series(1, a211.x) g on true;
select * from a211 full join lateral (select 1) s on true full join lateral (select a211.x) t on true;
drop table a211, b211;

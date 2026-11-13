-- A subquery reads an aggregate of the grouped query it is written in, which
-- also computes window functions over its groups.
create table w348 (g int, v int);
insert into w348 values (1, 10), (1, 20), (2, 5), (3, 7), (3, 8);
select sum(v), (select sum(v) + 1), row_number() over () from w348;
select g, sum(v), (select sum(v) * 2), rank() over (order by sum(v)) from w348 group by g order by g;
select g, exists (select 1 where count(*) > 1) as several, sum(count(*)) over () from w348 group by g order by g;
select g, (select max(x) from (values (sum(v)), (100)) s(x)), lag(sum(v)) over (order by g) from w348 group by g order by g;
select g, sum(v) in (select sum(v)) as same, count(*) over () from w348 group by g order by g;
select g from w348 group by g order by (select sum(v)), row_number() over ();
drop table w348;

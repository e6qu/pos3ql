-- An uncorrelated aggregate subquery over a view, beside a FROM item, reads
-- the view's expansion; a cast of an ARRAY constructor prints its elements
-- coerced to the element type.
create view v346 as select 1 as x union all select 2;
select g, (select count(*) from v346) from generate_series(1, 2) g;
select g, (select count(*) from (select * from v346) s) from generate_series(1, 2) g;
select g from generate_series(1, 3) g where g = (select count(*) from v346);
select g, 1 + (select count(*) from v346 limit 1) from generate_series(1, 2) g;
create type e346 as enum ('sad', 'ok');
create table t346 (m e346[] default array['sad', 'ok']::e346[], t text[] default array['a', 'b']::text[],
  i int[] default array[1, 2]::int[], b bigint[] default array[1, 2]::bigint[]);
select pg_get_expr(adbin, adrelid) from pg_attrdef where adrelid = 't346'::regclass order by adnum;
alter type e346 rename to e346r;
select pg_get_expr(adbin, adrelid) from pg_attrdef where adrelid = 't346'::regclass order by adnum;
insert into t346 default values returning *;
drop table t346; drop type e346r; drop view v346;

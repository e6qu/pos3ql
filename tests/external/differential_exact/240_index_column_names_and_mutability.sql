-- Generated index names join every key and INCLUDE column name, number
-- repeated names, and name expressions as a select list would (`expr` when
-- nothing names them). Index keys and predicates reject mutable functions.
create table k(a int, b text, c int);
create index on k (a, (a+1));
create index on k ((b::text));
create index on k ((c::text), b);
create index on k ((k.c));
create index on k (((k).c));
create index on k (lower(b) collate "C");
create index on k (a, a);
create index on k (a) include (b, c);
create index on k ((a+1), (a+2));
create index on k (lower(b), upper(b));
create index on k (coalesce(a, 0), (array[a]), ((a)::text), (-a), (a is null), (b::text));
select c.relname, a.attnum, a.attname from pg_class c join pg_attribute a on a.attrelid = c.oid
where c.relname like 'k\_%' and c.relkind = 'i' order by 1, 2;
select pg_get_indexdef(indexrelid) from pg_index where indrelid = 'k'::regclass order by 1;
create table k2 (like k including indexes);
create table k3 (a int, b text, c int);
create index on k3 (a, (a+1));
create index on k3 (lower(b), upper(b)) include (c);
create table k4 (like k3 including indexes);
select relname from pg_class where relname like 'k4%' and relkind = 'i' order by 1;
create table ki(a int, b text);
create function kv(int) returns int language sql volatile as 'select $1';
create function kimm(int) returns int language sql immutable as 'select $1';
create index on ki ((random()));
create index on ki ((now()));
create index on ki ((kv(a)));
create index on ki ((kimm(a)));
create index on ki (a) where kv(a) > 0;
create index on ki (a) where random() > 0;
create index on ki ((a + 1)) where kimm(a) > 0;
create index on ki ((lower(b)));
select indexrelid::regclass from pg_index where indrelid = 'ki'::regclass order by 1;
-- Statistics expressions are stored without their relation qualifier and
-- printed after the key columns, in attribute order.
create table st(a int, b int, c text);
create statistics st_s on (st.a + 1), (a + 1) from st;
create statistics st_s on (st.a * 2), c, lower(st.c), a from st;
alter table st rename to st2;
select stxname, pg_get_statisticsobjdef(oid), pg_get_statisticsobjdef_columns(oid),
  pg_get_statisticsobjdef_expressions(oid)
from pg_statistic_ext where stxrelid = 'st2'::regclass;
select (select stxname from pg_statistic_ext where oid = objid), refobjsubid, deptype
from pg_depend where classid = 'pg_statistic_ext'::regclass and refobjid = 'st2'::regclass
order by 1, 2;

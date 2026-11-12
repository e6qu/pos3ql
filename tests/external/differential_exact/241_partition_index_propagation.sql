-- A partition created or attached under a partitioned index gets that index:
-- an equivalent valid unattached index is attached, otherwise a copy is built
-- (checking existing rows), recursing into sub-partitions. Detaching makes
-- the copies independent again.
create table pt(a int, b int, c text) partition by range (a);
create unique index pt_a_idx on pt (a);
create index on pt (lower(c)) where b > 0;
create table pt1 partition of pt for values from (0) to (10);
insert into pt values (1, 1, 'x');
insert into pt values (1, 2, 'y');
create table pt2 (c text, b int, a int);
create index pt2_lower on pt2 (lower(c)) where b > 0;
alter table pt attach partition pt2 for values from (10) to (20);
insert into pt values (11, 1, 'x'), (11, 2, 'y');
create table pt3 (a int, b int, c text) partition by list (b);
create table pt31 partition of pt3 for values in (1);
alter table pt attach partition pt3 for values from (20) to (30);
create table pt4 (a int, b int, c text);
insert into pt4 values (35, 1, 'q'), (35, 1, 'q');
alter table pt attach partition pt4 for values from (30) to (40);
select relname from pg_class where relname like 'pt4%' order by 1;
select c.relname, c.relkind, i.indisvalid, i.indisunique, p.relname as parent, pg_get_indexdef(i.indexrelid)
from pg_index i join pg_class c on c.oid = i.indexrelid
left join pg_inherits h on h.inhrelid = i.indexrelid left join pg_class p on p.oid = h.inhparent
where i.indrelid in (select oid from pg_class where relname like 'pt%') order by 1;
alter table pt detach partition pt2;
select c.relname, p.relname from pg_class c left join pg_inherits h on h.inhrelid = c.oid
left join pg_class p on p.oid = h.inhparent where c.relname like 'pt2%' order by 1;
insert into pt2 values ('z', 1, 11);
drop table pt, pt2, pt3, pt4;
-- Sub-partitions, ON ONLY indexes and column positions that differ.
create table q(a int, b int) partition by range (a);
create index q_b on q (b);
create unique index q_ab on q (a, b);
create table q3 (a int, b int) partition by list (b);
create table q31 partition of q3 for values in (1);
create index q31_b on q31 (b);
alter table q attach partition q3 for values from (20) to (30);
create table q32 partition of q3 for values in (2);
create table q4 partition of q for values from (30) to (40) partition by list (b);
create table q41 partition of q4 for values in (1);
insert into q values (35, 1), (35, 1);
select c.relname, c.relkind, i.indisvalid, p.relname as parent
from pg_index i join pg_class c on c.oid = i.indexrelid
left join pg_inherits h on h.inhrelid = i.indexrelid left join pg_class p on p.oid = h.inhparent
where i.indrelid in (select oid from pg_class where relname like 'q%') order by 1;
drop table q;
create table r(a int) partition by range (a);
create table r1 partition of r for values from (0) to (10);
create index r_a on only r (a);
create table r2 partition of r for values from (10) to (20);
create table r3 (a int);
create index r3_a on r3 (a);
alter table r attach partition r3 for values from (20) to (30);
select c.relname, c.relkind, i.indisvalid, p.relname as parent
from pg_index i join pg_class c on c.oid = i.indexrelid
left join pg_inherits h on h.inhrelid = i.indexrelid left join pg_class p on p.oid = h.inhparent
where i.indrelid in (select oid from pg_class where relname like 'r%') order by 1;
create index r1_a on r1 (a);
alter index r_a attach partition r1_a;
select indisvalid from pg_index where indexrelid = 'r_a'::regclass;
drop index r3_a;
alter table r detach partition r3;
drop index r3_a;
drop table r, r3;
create table s(a int, b int) partition by range(a);
create table s1 (b int, a int);
alter table s1 drop column b;
alter table s1 add column b int;
alter table s attach partition s1 for values from (0) to (10);
create index on s (b) include (a);
create table s2 (a int, b int, x int);
alter table s2 drop column x;
create index s2_idx on s2 (b);
alter table s attach partition s2 for values from (10) to (20);
select c.relname, p.relname as parent, pg_get_indexdef(i.indexrelid)
from pg_index i join pg_class c on c.oid = i.indexrelid
left join pg_inherits h on h.inhrelid = i.indexrelid left join pg_class p on p.oid = h.inhparent
where i.indrelid in ('s'::regclass, 's1'::regclass, 's2'::regclass) order by 1;
drop table s;
-- CREATE INDEX on a partitioned table attaches equivalent partition indexes;
-- building a unique copy over duplicates fails as PostgreSQL's build does.
create table d(a int);
insert into d values (1), (1);
create unique index d_a on d (a);
create table dp(a int, b int) partition by range(a);
create table dp1 partition of dp for values from (0) to (10);
create index dp1_b on dp1 (b);
create index dp1_b2 on dp1 (b);
create index dp_b on dp (b);
insert into dp values (1, 1), (1, 1);
create unique index dp_a on dp (a);
create index dpx on dp (a);
drop index dp1_a_idx;
select c.relname, p.relname as parent from pg_index i join pg_class c on c.oid = i.indexrelid
left join pg_inherits h on h.inhrelid = i.indexrelid left join pg_class p on p.oid = h.inhparent
where i.indrelid in ('dp'::regclass, 'dp1'::regclass) order by 1;
drop table d, dp;
-- Rollback, savepoints, concurrent detach, temporary partitions.
create table m(a int, b int) partition by range (a);
create unique index m_a on m (a);
begin;
create table m3 partition of m for values from (20) to (30);
select relname from pg_class where relname like 'm3%' order by 1;
rollback;
select relname from pg_class where relname like 'm3%' order by 1;
begin;
create table m3 partition of m for values from (20) to (30);
savepoint s;
alter table m detach partition m3;
select c.relname, p.relname from pg_class c left join pg_inherits h on h.inhrelid = c.oid
left join pg_class p on p.oid = h.inhparent where c.relname like 'm3%' order by 1;
rollback to s;
select c.relname, p.relname from pg_class c left join pg_inherits h on h.inhrelid = c.oid
left join pg_class p on p.oid = h.inhparent where c.relname like 'm3%' order by 1;
commit;
insert into m values (25, 1), (25, 2);
alter table m detach partition m3 concurrently;
select c.relname, p.relname from pg_class c left join pg_inherits h on h.inhrelid = c.oid
left join pg_class p on p.oid = h.inhparent where c.relname like 'm3%' order by 1;
alter table m attach partition m3 for values from (20) to (30);
select c.relname, p.relname from pg_class c left join pg_inherits h on h.inhrelid = c.oid
left join pg_class p on p.oid = h.inhparent where c.relname like 'm3%' order by 1;
drop table m;
create temp table mt(a int, b int) partition by range (a);
create unique index on mt (a);
create temp table mt1 partition of mt for values from (0) to (10);
insert into mt values (1, 1), (1, 2);
drop table mt;
-- Foreign partitions take no index copies and refuse unique ones.
create foreign data wrapper w;
create server fs foreign data wrapper w;
create table f(a int, b int) partition by range (a);
create index f_b on f (b);
create foreign table f1 partition of f for values from (0) to (10) server fs;
create foreign table f2 (a int, b int) server fs;
alter table f attach partition f2 for values from (10) to (20);
create unique index f_a on f (a);
create table g(a int, b int) partition by range (a);
create unique index g_a on g (a);
create foreign table g1 partition of g for values from (0) to (10) server fs;
create foreign table g2 (a int, b int) server fs;
alter table g attach partition g2 for values from (10) to (20);
select c.relname from pg_index i join pg_class c on c.oid = i.indexrelid
where i.indrelid in (select oid from pg_class where relname in ('f', 'f1', 'f2', 'g', 'g2')) order by 1;
drop table f, g;
drop foreign table g2;
drop server fs;
drop foreign data wrapper w;

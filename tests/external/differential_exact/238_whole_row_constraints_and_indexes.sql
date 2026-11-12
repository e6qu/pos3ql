-- Whole-row references in CHECK constraints and index keys and predicates:
-- naming, conkey order, pg_depend rows, deparsed definitions, copies to new
-- relations, renames, and views passing the row to a function.
create table wr(a int, b text);
create function wr_f(wr) returns int language sql immutable as 'select $1.a';
create index wr_i1 on wr (wr_f(wr.*));
create index wr_i2 on wr (wr_f(wr));
alter table wr add check (wr_f(wr.*) > 0);
alter table wr add check (wr is not null);
alter table wr add check (row(wr.*) is not null);
create table wu(a int, check ((wu.*).a > 0), check (wu is not null), check ((wu).a < 100));
select conname, conkey, pg_get_constraintdef(oid) from pg_constraint
where conrelid in ('wr'::regclass, 'wu'::regclass) order by 1;
select pg_get_indexdef('wr_i1'::regclass), pg_get_indexdef('wr_i2'::regclass, 0, true);
select indexrelid::regclass, indkey from pg_index where indrelid = 'wr'::regclass order by 1;
select objid::regclass, refobjsubid, deptype from pg_depend
where refobjid = 'wr'::regclass and classid = 'pg_class'::regclass
  and objid::regclass::text not like 'pg_toast%' order by 1, 2, 3;
select (select conname from pg_constraint c where c.oid = objid), refobjsubid, deptype
from pg_depend where refobjid = 'wr'::regclass and classid = 'pg_constraint'::regclass
order by 1, 2, 3;
create table ww(a int, b int, c int, check (c > b and a > 0 and ww is not null));
select conname, conkey from pg_constraint where conrelid = 'ww'::regclass;
select refobjsubid, deptype from pg_depend
where refobjid = 'ww'::regclass and classid = 'pg_constraint'::regclass order by 1, 2;
create index wwi on ww (a, (ww is not null)) where ww.b > 0 and c > 0;
create index wwj on ww (c, (c + a), (a + c + b));
select indexrelid::regclass, indkey from pg_index where indrelid = 'ww'::regclass order by 1;
select objid::regclass, refobjsubid, deptype from pg_depend
where refobjid = 'ww'::regclass and classid = 'pg_class'::regclass
  and objid::regclass::text not like 'pg_toast%' order by 1, 2, 3;
alter table wr drop column b;
alter table ww drop column b;
select conname, pg_get_constraintdef(oid) from pg_constraint
where conrelid in ('wr'::regclass, 'ww'::regclass) order by 1;
select indexrelid::regclass, pg_get_indexdef(indexrelid) from pg_index
where indrelid in ('wr'::regclass, 'ww'::regclass) order by 1;
create view wv as select wr_f(wr.*) as by_star, wr_f(wr) as by_name from wr;
select pg_get_viewdef('wv');
alter table wr add column c int;
insert into wr values (1);
insert into wr values (-1, 2);
insert into wr values (2, 2);
select * from wv;
select * from wr where wr_f(wr) = 2;
create statistics wr_s1 on (wr_f(wr.*)), (a + 1) from wr;
create statistics wr_s2 on (wr is null), a from wr;
-- Qualifiers must name the constrained relation.
create table wbad1(a int, check (zz.* is not null));
create table wbad2(a int, check (zz.a > 0));
create table wbad3(a int, check (zz is not null));
create table wbad4(a int, check ((wbad4).zz > 0));
create table wq(a int, b text, check (wq.a > 0), check ((wq).b <> ''));
create table wk(a int, b text);
create index on wk ((zz.*));
create index on wk (wk);
create index on wk ((zz));
create index on wk (((wk).a));
create index on wk ((wk.a + 1)) where wk is not null;
create index on wk ((wk.b)) include (a);
select pg_get_indexdef(indexrelid) from pg_index where indrelid = 'wk'::regclass order by 1;
-- Copies keep qualified columns and refuse whole-row references.
create table wq_like (like wq including constraints);
insert into wq_like values (1, 'x');
create table wq_child () inherits (wq);
insert into wq_child values (1, 'x');
select conname, pg_get_constraintdef(oid) from pg_constraint
where conrelid in ('wq_like'::regclass, 'wq_child'::regclass) order by 1;
create table wr_like (like wr including constraints);
create table wr_child () inherits (wr);
create table wr_copy (like wr);
create table wk_index_copy (like wk including indexes);
create table wpart(a int, b text, check (wpart is not null)) partition by list (a);
create table wpart1 partition of wpart for values in (1);
create table wpidx(a int, b text) partition by list (a);
create table wpidx1 partition of wpidx for values in (1);
create index on wpidx ((wpidx.*));
-- A rename follows the relation in its CHECK constraints.
create index wq_plain on wq ((wq.a + 1)) where wq.b <> '';
alter table wq rename to wq2;
insert into wq2 values (1, 'y');
select pg_get_indexdef('wq_plain'::regclass);
drop index wq_plain;
alter table wq2 rename column a to aa;
insert into wq2 values (2, 'y');
select conname, pg_get_constraintdef(oid) from pg_constraint
where conrelid = 'wq2'::regclass order by 1;
create table wn(a int, b text, check (wn is not null));
alter table wn rename to wn2;
insert into wn2 values (1, 'z');
insert into wn2 values (1, null);
select conname, pg_get_constraintdef(oid) from pg_constraint where conrelid = 'wn2'::regclass;
-- Row null tests read every field of a table or composite row.
create type wcp as (a int, b int);
create table wnull(a int, b text, c wcp);
insert into wnull values (1, null, row(1, null)), (1, 'x', row(1, 2)), (null, null, null),
  (null, null, row(null, null));
select wnull is not null, wnull is null, (wnull.*) is null, c is null, c is not null,
  row(1, null)::wcp is not null, (c).a is null from wnull order by a, b;
select * from wnull where c is not null order by 1, 2;
-- A whole-row target in a view's select list keeps its row-type cast.
create table wtop(a int, b text);
create view wtop_v as select wtop, a from wtop;
select pg_get_viewdef('wtop_v');

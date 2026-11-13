-- CHECK ... NO INHERIT constraints stay local to their table.
create table np (a int, constraint pc check (a > 0) no inherit, constraint qc check (a < 100));
create table nc () inherits (np);
select conrelid::regclass, conname, connoinherit, conislocal, coninhcount, pg_get_constraintdef(oid)
from pg_constraint where conrelid::regclass::text in ('np', 'nc') and contype = 'c' order by 1, 2;
insert into nc values (-1);
insert into np values (-1);
alter table np add constraint rc check (a <> 5) no inherit not valid;
alter table np add constraint sc check (a <> 6) not valid no inherit;
alter table only np add constraint tc check (a <> 7) no inherit;
select conrelid::regclass, conname, connoinherit, convalidated, pg_get_constraintdef(oid)
from pg_constraint where conrelid::regclass::text in ('np', 'nc') and contype = 'c' order by 1, 2;
alter table np drop constraint pc;
alter table np rename constraint rc to rc2;
select conrelid::regclass, conname from pg_constraint
where conrelid::regclass::text in ('np', 'nc') and contype = 'c' order by 1, 2;
create table nc2 (a int, constraint qc check (a < 100));
alter table nc2 inherit np;
create table npt (a int, check (a > 0) no inherit) partition by list (a);
create table npt (a int) partition by list (a);
alter table npt add constraint x check (a > 0) no inherit;
create table nd (a int check (a > 0) no inherit no inherit);
create table nd (a int check (a > 0) no inherit);
select conname, connoinherit, pg_get_constraintdef(oid) from pg_constraint
where conrelid = 'nd'::regclass and contype = 'c';
create domain ndd as int check (value > 0) no inherit;
\d np
drop table np cascade;
drop table npt, nd;

-- An inherited NOT NULL constraint keeps its parent's name; a child that
-- declares the constraint locally keeps its own.
create table p (k int not null, j int constraint jnn not null);
create table c () inherits (p);
create table c2 (k int not null) inherits (p);
create table c3 (k int constraint c3_own not null, extra int not null) inherits (p);
create table pp (k int not null, v int) partition by list (k);
create table pp1 partition of pp for values in (1);
create table pp2 (k int not null, v int);
alter table pp attach partition pp2 for values in (2);
select conrelid::regclass, conname, conislocal, coninhcount from pg_constraint
where contype = 'n' and conrelid::regclass::text in ('p', 'c', 'c2', 'c3', 'pp', 'pp1', 'pp2')
order by 1, 2;
alter table p add column z int not null default 0;
alter table p add column y int constraint ynn not null default 0;
select conrelid::regclass, conname from pg_constraint
where contype = 'n' and conrelid::regclass::text in ('p', 'c', 'c2', 'c3') order by 1, 2;
\d c
drop table p cascade;
drop table pp cascade;
-- ALTER TABLE ... INHERIT checks each parent column.
create table p (k int not null, j text collate "C");
create table c4 (j text collate "C");
alter table c4 inherit p;
create table c5 (k bigint, j text collate "C");
alter table c5 inherit p;
create table c6 (k int not null no inherit, j text collate "C");
alter table c6 inherit p;
create table c7 (k int not null, j text collate "POSIX");
alter table c7 inherit p;
drop table p, c4, c5, c6, c7;

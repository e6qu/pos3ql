-- ATTACH PARTITION attaches a partition's equivalent primary key or unique
-- constraint to its parent's index instead of building a second index.
create table p349 (a int, b int) partition by range (a);
create unique index p349_a on p349 (a);
create table c349 (a int, b int, constraint c349_uq unique (a));
alter table p349 attach partition c349 for values from (0) to (10);
select c.relname, i.inhparent::regclass from pg_inherits i
  join pg_class c on c.oid = i.inhrelid where c.relname like '%349%' order by 1;
\d c349
select objid::regclass, refobjid::regclass, deptype from pg_depend
 where classid = 'pg_class'::regclass and refclassid = 'pg_class'::regclass
   and objid = 'c349_uq'::regclass order by 2, 3;
drop index p349_a;
alter table c349 drop constraint c349_uq;
drop index c349_uq;
alter table p349 detach partition c349;
select c.relname, i.inhparent::regclass from pg_inherits i
  join pg_class c on c.oid = i.inhrelid where c.relname like '%349%' order by 1;
alter table p349 attach partition c349 for values from (0) to (10);
drop index p349_a cascade;
\d c349
drop table p349;

-- A parent's primary key takes the partition's own primary key.
create table q349 (a int, b int, primary key (a)) partition by range (a);
create table qc349 (a int not null, b int, constraint qc349_pk primary key (a));
alter table q349 attach partition qc349 for values from (0) to (10);
\d qc349
select con.conname, con.conparentid <> 0 as inherited, con.conislocal, con.coninhcount
  from pg_constraint con where con.conrelid = 'qc349'::regclass and con.contype = 'p';
select dep.deptype, ref.conname from pg_depend dep
  join pg_constraint con on con.oid = dep.objid
  join pg_constraint ref on ref.oid = dep.refobjid
 where dep.classid = 'pg_constraint'::regclass and con.conname = 'qc349_pk';
alter table qc349 drop constraint qc349_pk;
drop index qc349_pk;
alter table q349 detach partition qc349;
select conname, conparentid <> 0 from pg_constraint where conrelid = 'qc349'::regclass and contype = 'p';
drop table q349, qc349;

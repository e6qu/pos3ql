-- ON DELETE SET NULL (columns) and SET DEFAULT (columns) change only the
-- named referencing columns (PostgreSQL 15+, pg_constraint.confdelsetcols).
CREATE SCHEMA delete_set_column_lists;
SET search_path = delete_set_column_lists, public;
create table tenants(tenant int, id int, primary key (tenant, id));
create table docs(id int, tenant int default 0, owner int default 0,
  foreign key (tenant, owner) references tenants on delete set null (owner));
create table notes(id int, tenant int default 0, owner int default 0,
  foreign key (tenant, owner) references tenants on delete set default (owner));
insert into tenants values (1, 10), (1, 11), (1, 0), (0, 0);
insert into docs values (1, 1, 10), (2, 1, 11);
insert into notes values (1, 1, 10), (2, 1, 11);
delete from tenants where id = 10;
select * from docs order by id;
select * from notes order by id;
-- The list keeps its written order and drops repeats.
alter table docs add constraint docs_repeat foreign key (tenant, owner)
  references tenants on delete set null (owner, tenant, owner);
select conname, confdelsetcols, confdeltype, confupdtype
  from pg_constraint where conrelid in ('docs'::regclass, 'notes'::regclass) order by conname;
select conname, pg_get_constraintdef(oid) from pg_constraint
  where conrelid in ('docs'::regclass, 'notes'::regclass) order by conname;
-- Errors PostgreSQL reports for the list.
create table bad1(id int, a int, b int, foreign key (a, b) references tenants on update set null (a));
create table bad2(id int, a int, b int, foreign key (a, b) references tenants on update set default (b));
create table bad3(id int, a int, b int, foreign key (a, b) references tenants on delete set null (id));
create table bad4(id int, a int, b int, foreign key (a, b) references tenants on delete set null (zz));
create table bad5(id int, a int, b int, foreign key (a, b) references tenants on delete cascade (a));
create table bad6(id int, a int, b int, foreign key (a, b) references tenants on delete set null ());
create table bad7(id int, a int, b int, foreign key (zz, b) references tenants);
create table bad8(id int, a int, b int, foreign key (a, b) references tenants (tenant, zz));
-- The column list follows the key when an earlier column is dropped.
create table shifted(junk int, id int, tenant int, owner int,
  foreign key (tenant, owner) references tenants on delete set null (owner));
insert into shifted values (0, 1, 1, 11);
alter table shifted drop column junk;
delete from tenants where id = 11;
select * from shifted;
select pg_get_constraintdef(oid) from pg_constraint where conrelid = 'shifted'::regclass;
-- Partitions inherit the list.
delete from notes;
create table parted(id int, tenant int, owner int,
  foreign key (tenant, owner) references tenants on delete set null (owner))
  partition by range (id);
create table parted_low partition of parted for values from (0) to (100);
insert into parted values (1, 1, 0);
delete from tenants where tenant = 1 and id = 0;
select * from parted;
select conrelid::regclass::text, confdelsetcols from pg_constraint
  where conrelid in ('parted'::regclass, 'parted_low'::regclass) order by 1;
-- A SET DEFAULT list whose defaults still name the deleted key fails.
create table keep(id int, tenant int default 1, owner int default 12,
  foreign key (tenant, owner) references tenants on delete set default (tenant));
insert into tenants values (1, 12), (2, 12);
insert into keep values (1, 2, 12);
delete from tenants where tenant = 2;
select * from keep;
delete from tenants where tenant = 1 and id = 12;
DROP SCHEMA delete_set_column_lists CASCADE;

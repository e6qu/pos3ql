-- DROP SCHEMA ... CASCADE reports a partition dropped with its parent as
-- part of that table, as PostgreSQL's partition dependencies do; a partition
-- whose parent lives in another schema is reported on its own.
CREATE SCHEMA drop_schema_partitions;
create table drop_schema_partitions.pt(a int, b text) partition by range(a);
create table drop_schema_partitions.pt1 partition of drop_schema_partitions.pt for values from (0) to (10);
create table drop_schema_partitions.pt2 partition of drop_schema_partitions.pt
  for values from (10) to (20) partition by list(b);
create table drop_schema_partitions.pt2a partition of drop_schema_partitions.pt2 for values in ('x');
create table drop_schema_partitions.inh_parent(a int);
create table drop_schema_partitions.inh_child() inherits (drop_schema_partitions.inh_parent);
create index on drop_schema_partitions.pt(a);
create view drop_schema_partitions.v as select * from drop_schema_partitions.pt;
CREATE SCHEMA drop_schema_partitions_leaf;
create table drop_schema_partitions_leaf.pt3 partition of drop_schema_partitions.pt for values from (20) to (30);
drop schema drop_schema_partitions_leaf;
drop schema drop_schema_partitions;
DROP SCHEMA drop_schema_partitions_leaf CASCADE;
DROP SCHEMA drop_schema_partitions CASCADE;

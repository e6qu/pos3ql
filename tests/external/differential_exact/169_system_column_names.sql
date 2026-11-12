-- A table's columns cannot take a system column's name; views can.
CREATE SCHEMA system_column_names;
SET search_path = system_column_names, public;
create table sc1(tableoid int);
create table sc2(a int, "xmin" text);
create table sc3(a int);
alter table sc3 add column ctid int;
alter table sc3 add column if not exists xmax int;
alter table sc3 rename column a to cmax;
alter table sc3 rename column a to cmin;
create table sc4 as select 1 as xmax;
create table sc5(oid int, a int);
create view sv as select 1 as ctid, 2 as xmin;
select ctid, xmin from sv;
select s.ctid from sv s;
select * from sc3;

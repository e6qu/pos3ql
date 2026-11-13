-- A partitioned table records no access method unless USING names one, and
-- its partitions take its method or default_table_access_method.
create access method am340 type table handler heap_tableam_handler;
create table p340 (a int) partition by list (a) using am340;
create table p340_1 partition of p340 for values in (1);
create table p340_2 partition of p340 for values in (2) using heap;
create table q340 (a int) partition by list (a);
create table q340_1 partition of q340 for values in (1);
create table q340_2 (a int);
alter table q340 attach partition q340_2 for values in (2);
select c.relname, c.relkind, a.amname from pg_class c left join pg_am a on a.oid = c.relam
where c.relname like '_340%' order by 1;
alter table q340 set access method am340;
create table q340_3 partition of q340 for values in (3);
alter table p340 set access method default;
create table p340_3 partition of p340 for values in (3);
alter table q340_1 set access method default;
select c.relname, a.amname from pg_class c left join pg_am a on a.oid = c.relam
where c.relname in ('p340', 'p340_3', 'q340', 'q340_1', 'q340_3') order by 1;
\d+ p340
\d+ q340
drop table p340, q340;
drop access method am340;
-- Domain constraints print VALUE and coerce domain operands to their base.
create domain d340 as int check (value > 0) check (value < 100 and value <> 5) not null;
create domain d340_even as d340 check (value % 2 = 0);
create domain d340_text as text check (length(value) > 1 and value like 'a%');
\dD d340*
select pg_get_constraintdef(oid), pg_get_constraintdef(oid, true) from pg_constraint
where contypid in ('d340'::regtype, 'd340_even'::regtype) order by conname;
create table t340 (c d340_even check (c % 4 = 0), v varchar check (v like 'a%'));
select pg_get_constraintdef(oid, true) from pg_constraint where conrelid = 't340'::regclass order by conname;
drop table t340;
drop domain d340_even, d340, d340_text;
-- initdb's template database and public schema ACLs.
select datname, datacl from pg_database where datname like 'template%' order by 1;
create role r340;
grant create on schema public to r340;
select nspacl from pg_namespace where nspname = 'public';
revoke create on schema public from r340;
select nspacl from pg_namespace where nspname = 'public';
drop role r340;
create extension if not exists plpgsql;

-- Key constraints are non-inheritable unless they are a partition's copy of
-- the parent's; enum input errors name a type off the search path qualified.
create table kc (k int primary key, u int unique);
create table kc_part (k int primary key) partition by range (k);
create table kc_part1 partition of kc_part for values from (0) to (10);
select conname, connoinherit, conislocal, coninhcount from pg_constraint
  where conrelid in ('kc'::regclass, 'kc_part'::regclass, 'kc_part1'::regclass)
  and contype in ('p', 'u') order by conname;
create schema kc_s;
create type kc_s.e as enum ('a');
create table kc_e (x kc_s.e, t text);
insert into kc_e values ('a', 'c');
select t::kc_s.e from kc_e;
drop table kc, kc_part, kc_e;
drop schema kc_s cascade;

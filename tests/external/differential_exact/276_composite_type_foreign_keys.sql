-- A foreign key between two different composite types compares the values
-- field by field (PostgreSQL's record_eq through the record operator class),
-- and two composite values compare that way in SQL too: two NULL fields are
-- equal, and fields must have the same types.
CREATE SCHEMA composite_type_foreign_keys;
SET search_path = composite_type_foreign_keys, public;
create type pair as (a int, b text);
create type other_pair as (x int, y text);
create type wide_pair as (x bigint, y varchar);
create type int_pair as (x int, y int);
create type single as (x int);
select (1,'x')::pair = (1,'x')::other_pair as same, (1,'x')::pair = (1,'y')::other_pair as differ,
  (1,'x')::pair <> (1,'y')::other_pair as ne, (1,'x')::pair < (2,'a')::other_pair as lt;
select (1,null)::pair = (1,null)::pair as null_fields, (1,null)::pair = (1,'x')::other_pair as one_null,
  (2,null)::pair > (2,'z')::other_pair as null_sorts_last;
select (1,'x')::pair = (1,'x')::wide_pair;
select (1,'x')::pair = (1,2)::int_pair;
select (1,'x')::pair = (2,2)::int_pair;
select (1,'x')::pair = row(1)::single;
create table stored_pairs(p pair, o other_pair);
insert into stored_pairs values ((1,null), (1,null)), ((1,'x'), (1,'y'));
select count(*) from stored_pairs where p = o;
select count(*) from stored_pairs a join stored_pairs b on a.p = b.p;
create table parents(k pair primary key);
create table children(k other_pair references parents on delete cascade);
create table wide_children(k wide_pair references parents);
create table int_children(k int_pair references parents);
select conname, conpfeqop, conppeqop, conffeqop from pg_constraint
  where contype = 'f' and connamespace = 'composite_type_foreign_keys'::regnamespace order by 1;
insert into parents values ((1,'x')), ((2,'y')), ((3,null));
insert into children values ((1,'x')), ((3,null));
insert into children values ((4,'z'));
insert into wide_children values ((1,'x'));
insert into int_children values ((1,2));
delete from parents where k = (1,'x')::pair;
select * from children order by k;
delete from parents where (k).a = 3;
select count(*) from children;
create table restricted(k other_pair references parents on delete restrict);
insert into restricted values ((2,'y'));
delete from parents where (k).a = 2;
DROP SCHEMA composite_type_foreign_keys CASCADE;

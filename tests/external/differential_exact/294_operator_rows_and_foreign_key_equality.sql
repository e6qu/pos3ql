-- pg_operator lists every built-in operator, so a foreign key's equality
-- operator lists resolve like PostgreSQL's, including keys whose types
-- coerce (varchar to text) or are domains.
select count(*), count(*) filter (where oprname = '=') from pg_operator where oid < 16384;
select oid, oprname, oprleft::regtype, oprright::regtype, oprresult::regtype, oprcom, oprnegate,
       oprcode, oprrest, oprjoin, oprcanmerge, oprcanhash
from pg_operator where oid in (15, 96, 98, 410, 416, 1054, 2347, 3516, 1070) order by oid;
select count(*) from pg_operator o left join pg_proc p on p.oid = o.oprcode where p.oid is null;
select oid, oid::regoper as name, oid::regoperator as signature from pg_operator
where oid in (15, 96, 484, 1209, 2860, 3882, 3897, 4012, 3966) order by oid;
select oprname, oid::regoper as name from pg_operator where oprname in ('@?', '@@@', '<^', '?-|') order by oid;
create type mood as enum ('sad', 'happy');
create domain posint as int check (value > 0);
create table pk_int (k int primary key, t text unique, v varchar(10) unique, e mood unique,
                     d date unique, n numeric unique, b bigint unique, a int[] unique);
create table fk_types (
  i int references pk_int (k),
  id posint references pk_int (k),
  t text references pk_int (t),
  vt varchar(5) references pk_int (t),
  tv text references pk_int (v),
  e mood references pk_int (e),
  dd date references pk_int (d),
  nn numeric references pk_int (n),
  bb bigint references pk_int (b),
  ai int[] references pk_int (a));
select conname, conpfeqop, conppeqop, conffeqop from pg_constraint
where conrelid = 'fk_types'::regclass and contype = 'f' order by conname;
create table pk_two (a int, b text, primary key (a, b));
create table fk_two (a int, b varchar, foreign key (a, b) references pk_two);
select conname, conpfeqop, conppeqop, conffeqop from pg_constraint
where conrelid = 'fk_two'::regclass and contype = 'f';
select conname, conpfeqop::regoperator[] from pg_constraint where conrelid = 'fk_two'::regclass and contype = 'f';
drop table fk_two, pk_two, fk_types, pk_int;
drop domain posint;
drop type mood;
-- format_type names built-in types without an executable representation.
select oid, format_type(oid, null), oid::regtype as type from pg_type
where oid in (22, 30, 71, 210, 705, 1006, 1009, 2276, 2283, 3831, 4537, 32, 5069) order by oid;
select count(*) from pg_type where oid < 13000 and format_type(oid, null) like '%?%';

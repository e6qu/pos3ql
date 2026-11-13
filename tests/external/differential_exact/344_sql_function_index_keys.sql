-- Index keys, CHECK constraints, defaults and generated columns calling SQL
-- functions with RETURN, BEGIN ATOMIC and string bodies, whose inlined bodies
-- decide an index key's mutability.
create function f13(int) returns int language sql immutable return $1;
create function g13(x int) returns int language sql immutable return x + 1;
create function h13(int) returns int language sql immutable as 'select $1 * 2';
create table t13 (a int);
create index on t13 (f13(a));
create index on t13 (g13(a));
create index on t13 (h13(a));
insert into t13 values (1), (2);
select a from t13 where f13(a) = 2 and g13(a) = 3 and h13(a) = 4;
create table t13b (a int check (f13(a) > 0), b int default g13(1), c int generated always as (h13(a)) stored);
insert into t13b (a) values (5);
select * from t13b;
drop table t13, t13b; drop function f13(int), g13(int), h13(int);
create function s13(x int) returns int language sql stable return x + 1;
create function a13(x int) returns int language sql stable begin atomic select x + 1; end;
create function v13(x int) returns int language sql stable return x + random()::int;
create table t13c (a int);
create index on t13c (s13(a));
create index on t13c (a13(a));
create index on t13c (v13(a));
drop table t13c; drop function s13(int), a13(int), v13(int);

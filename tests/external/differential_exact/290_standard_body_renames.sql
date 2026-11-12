-- SQL-standard bodies are bound to the routines and types they named at
-- CREATE: renaming or moving those objects keeps the results and prints the
-- current names.
create schema sb;
create schema sb_moved;
create function sb.f(int) returns int language sql immutable as 'select $1 + 1';
create type sb.e as enum ('a', 'b');
create function sb_select() returns int language sql begin atomic select sb.f(1); end;
create function sb_return() returns int language sql return sb.f(2);
create function sb_enum() returns sb.e language sql return 'b'::sb.e;
create function sb_cast() returns text language sql begin atomic select 'a'::sb.e::text; end;
create function sb_named(x int) returns int language sql begin atomic select abs(x); end;
alter function sb.f(int) rename to f2;
alter type sb.e rename to e2;
select sb_select(), sb_return(), sb_enum(), sb_cast();
select pg_get_functiondef('sb_select'::regproc);
select pg_get_function_sqlbody('sb_named'::regproc);
alter function sb.f2(int) set schema sb_moved;
alter type sb.e2 set schema sb_moved;
select sb_select(), sb_return(), sb_enum(), sb_cast();
select proname, pg_get_function_sqlbody(oid) from pg_proc
  where proname in ('sb_select', 'sb_return', 'sb_enum', 'sb_cast') order by proname;
drop function sb_select(), sb_return(), sb_enum(), sb_cast(), sb_named(int);
drop schema sb cascade;
drop schema sb_moved cascade;

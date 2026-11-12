-- pg_event_trigger_dropped_objects lists a cascade in the order of the
-- dependency walk's report; pg_log_backend_memory_contexts answers for a
-- server process and warns for any other PID.
create table walk_log(n serial, object_type text, original boolean, normal boolean, identity text);
create function walk_drop() returns event_trigger language plpgsql as $$
declare r record;
begin
  for r in select * from pg_event_trigger_dropped_objects() loop
    if r.object_type not in ('toast table') and r.object_identity not like 'pg_toast.%' then
      insert into walk_log(object_type, original, normal, identity)
      values (r.object_type, r.original, r.normal, r.object_identity);
    end if;
  end loop;
end $$;
create event trigger walk_drop on sql_drop execute function walk_drop();
create table walk_t(a int primary key, b text);
create materialized view walk_m as select * from walk_t;
create view walk_v as select * from walk_m;
create view walk_v2 as select a from walk_t;
create index walk_i on walk_t (b);
drop table walk_t cascade;
create type walk_c as (x int, y text);
create table walk_u(c walk_c, d int);
create view walk_uv as select (c).x from walk_u;
create function walk_f(walk_c) returns int language sql as 'select $1.x';
drop type walk_c cascade;
create schema walk_s;
create table walk_s.t1(a int);
create view walk_s.v1 as select * from walk_s.t1;
create sequence walk_s.q1;
drop schema walk_s cascade;
drop event trigger walk_drop;
select object_type, original, normal, identity from walk_log order by n;
select pg_log_backend_memory_contexts(pg_backend_pid());
select pg_log_backend_memory_contexts(1);
select pg_log_backend_memory_contexts(null);
create role walk_r;
set role walk_r;
select pg_log_backend_memory_contexts(1);
reset role;
drop role walk_r;

-- A SQL function's final query is checked against its resolved result type
-- when called: a polymorphic result with the call's argument types, and a
-- body created with check_function_bodies off.
create function poly287(x anyelement) returns anyelement language sql as $$ select 'abc'::text $$;
select poly287(1);
select poly287('x'::text);
create function poly2872(x anyelement) returns anyelement language sql as $$ select 1 $$;
select poly2872(1.5);
select poly2872(2);
create function poly2873(x anyelement) returns anyarray language sql as $$ select array['a'] $$;
select poly2873(1);
create function poly2874(x anyelement) returns anyelement language sql as $$ select x from (values (1)) v $$;
select poly2874(5);
create function poly2875(x anycompatible, y anycompatible) returns anycompatible language sql as $$ select 'q'::text $$;
select poly2875(1, 2.5);
create function poly2876(x anyelement) returns anyelement language sql as $$ select 1; select 'z'::text $$;
select poly2876(3);
create function poly2877(x anyelement) returns anyelement language sql as $$ select x $$;
select poly2877(3), poly2877('a'::text), poly2877(null::int);
select poly287(x) from (values (1), (2)) v(x);
create function poly2878(x anyelement) returns anyelement language sql as $$ select 'q'::text from (values (1)) v $$;
select poly2878(3);
set check_function_bodies = off;
create function plain287(x int) returns int language sql as $$ select 'q'::text $$;
select plain287(1);

-- pg_cursors shows a declared cursor's whole client query string, kept
-- while the cursor is held; a procedural OPEN shows its query.
begin;
declare c1_288 cursor for select 1;
declare c2_288 cursor with hold for select 2;   select name, statement from pg_cursors order by name;
create function op288() returns refcursor language plpgsql as $$ declare r refcursor := 'pc288'; begin open r for select 3; return r; end $$;
select op288();
select name, statement from pg_cursors order by name;
commit;
select name, statement, is_holdable from pg_cursors order by name;
fetch all from c2_288;
select name, statement from pg_cursors order by name;

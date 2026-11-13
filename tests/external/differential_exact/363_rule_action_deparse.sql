-- Rule actions of every common shape print as PostgreSQL deparses them in
-- both pg_get_ruledef modes: INSERT ... SELECT, multi-row VALUES, joined
-- UPDATE and DELETE, SELECT actions and RETURNING lists.
create table t363(a int, b text);
create table l363(a int, b text);
create table x363(a int, c text);
create rule _1 as on insert to t363 do also insert into l363 select new.a, new.b;
create rule _2 as on insert to t363 do also insert into l363 values (new.a, 'x'), (2, new.b);
create rule _3 as on update to t363 do also update l363 set b = x363.c from x363 where l363.a = x363.a and x363.a = new.a;
create rule _4 as on delete to t363 do also delete from l363 using x363 where l363.a = x363.a and x363.a = old.a;
create rule _5 as on insert to l363 do instead insert into x363 values (new.a, new.b) returning x363.a, x363.c;
create rule _6 as on update to x363 do instead update l363 set b = new.c where a = old.a returning a, b;
create rule _7 as on delete to x363 do instead delete from l363 where a = old.a returning *;
create rule _8 as on insert to x363 do also insert into l363 select a, b from t363 where a > new.a;
create rule _9 as on update to l363 do also select new.a, old.b;
select rulename, pg_get_ruledef(oid) from pg_rewrite where rulename ~ '^r363_[0-9]$' order by 1;
select rulename, pg_get_ruledef(oid, true) from pg_rewrite where rulename ~ '^r363_[0-9]$' order by 1;
create rule _1 as on insert to t363 do also insert into l363 select new.a + 1 as a, upper(new.b) as b;
create rule _2 as on insert to t363 do also insert into l363 (b, a) values ('q', new.a), (DEFAULT, 3);
create rule _3 as on update to t363 do also update l363 set a = l363.a + 1, b = 'z' from x363 where l363.a = x363.a;
create rule _4 as on delete to t363 do also insert into l363 select x363.a, x363.c from x363 where x363.c = old.b;
create rule _5 as on insert to x363 do also select new.a + 1 as x, new.c from t363 where t363.a = new.a;
create rule _6 as on update to x363 do instead insert into l363 values (new.a, new.c) returning l363.a + 1 as a, b;
select rulename, pg_get_ruledef(oid) from pg_rewrite where rulename ~ '^s363_[0-9]$' order by 1;
select rulename, pg_get_ruledef(oid, true) from pg_rewrite where rulename ~ '^s363_[0-9]$' order by 1;
SELECT rulename, definition FROM pg_rules WHERE tablename = 'x363' ORDER BY rulename;
DROP TABLE t363, l363, x363 CASCADE;

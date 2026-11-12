-- A BEFORE trigger's SELECT ... INTO and EXECUTE ... INTO assign fields of NEW.
create table n196(a int, b text);
create function n196f() returns trigger language plpgsql as $$
begin
  select 42, 'x' || new.b into new.a, new.b;
  execute 'select $1 * 2' into new.a using new.a;
  select '12' into new.a;
  select new.a + 1.6 into new.a;
  return new;
end $$;
create trigger n196_t before insert on n196 for each row execute function n196f();
insert into n196 values (1, 'q');
select * from n196;
create function n196bad() returns int language plpgsql as $$ begin select 1 into new.a; return 1; end $$;
create function n196missing() returns trigger language plpgsql as $$
begin
  select 6 into new.zz;
  return new;
end $$;
create table m196(a int);
create trigger m196_t before insert on m196 for each row execute function n196missing();
insert into m196 values (1);
drop table n196, m196;
drop function n196f(), n196missing();

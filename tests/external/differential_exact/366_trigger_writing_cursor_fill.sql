-- A trigger fired inside a writing statement may first read a session cursor
-- whose data-modifying WITH queries have not run: they run then, and the
-- cursor holds the main query's rows.
create table log366(x int);
create table t366(a int);
create function open366() returns refcursor language plpgsql as $$
declare c refcursor := 'cur';
begin
  open c for with w as (insert into log366 values (1), (2) returning x) select x * 10 as y from w;
  return c;
end $$;
create function trg366() returns trigger language plpgsql as $$
declare r record; c refcursor := 'cur';
begin
  raise notice 'log rows before %', (select count(*) from log366);
  fetch c into r;
  raise notice 'fetched %', r.y;
  raise notice 'log rows after %', (select count(*) from log366);
  return new;
end $$;
create trigger t366r after insert on t366 for each row execute function trg366();
begin;
select open366();
insert into t366 values (1);
select * from log366 order by x;
fetch all from cur;
commit;
begin;
select open366();
insert into t366 select generate_series(1, 2);
fetch all from cur;
commit;
CREATE TABLE b366 (a int);
CREATE FUNCTION btrg366() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE n bigint;
BEGIN
  EXECUTE 'FETCH cur' INTO n;
  RAISE NOTICE 'dynamic fetch %', n;
  RETURN new;
END $$;
CREATE TRIGGER btr366 BEFORE INSERT ON b366 FOR EACH ROW EXECUTE FUNCTION btrg366();
BEGIN;
SELECT open366();
INSERT INTO b366 VALUES (1);
SELECT count(*) FROM log366;
COMMIT;
DROP TABLE log366, t366, b366 CASCADE;
DROP FUNCTION trg366(), open366(), btrg366();

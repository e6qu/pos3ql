-- Index keys and predicates must be immutable after SQL-function inlining:
-- casts and operators PostgreSQL implements with stable functions, and
-- non-inlinable non-immutable user functions, are refused with 42P17.
-- A SQL-standard body implies LANGUAGE SQL.
create table mu(stamp timestamptz, ts timestamp, d date, t text, n int, iv interval);
create index on mu ((stamp::date));
create index on mu ((ts::date));
create index on mu ((ts::timestamptz));
create index on mu ((stamp::timestamp));
create index on mu ((d::timestamptz));
create index on mu ((t::date));
create index on mu ((t::int));
create index on mu ((n::text));
create index on mu ((stamp::text));
create index on mu ((ts + iv));
create index on mu ((stamp + iv));
create index on mu ((stamp > '2020-01-01'));
create index on mu (n) where stamp::date > '2020-01-01';
create index on mu (n) where ts::date > '2020-01-01';
create index on mu ((date_trunc('day', stamp)));
create index on mu ((date_trunc('day', ts)));
create index on mu ((to_char(n, '999')));
create index on mu ((lower(t)));
create function fs(int) returns int language plpgsql stable as 'begin return $1; end';
create function fi(int) returns int language plpgsql immutable as 'begin return $1; end';
create function sv(int) returns int language sql volatile as 'select $1';
create function sv2(int) returns int language sql volatile as 'select $1 + extract(day from now())::int';
create function ss(timestamptz) returns date language sql stable as 'select $1::date';
create function si(timestamptz) returns date language sql immutable as 'select $1::date';
create function sd(int) returns int language sql stable security definer as 'select $1';
create function sm(int) returns int language sql stable as 'select 1; select $1';
create function sa(int) returns int stable begin atomic select $1; end;
create index on mu ((fs(n)));
create index on mu ((fi(n)));
create index on mu ((sv(n)));
create index on mu ((sv2(n)));
create index on mu ((ss(stamp)));
create index on mu ((si(stamp)));
create index on mu ((sd(n)));
create index on mu ((sm(n)));
create index on mu ((sa(n)));
create index on mu (n) where fs(n) > 0;
create index on mu (n) where sv(n) > 0;
select indexrelid::regclass from pg_index where indrelid = 'mu'::regclass order by 1;
drop table mu;
drop function fs, fi, sv, sv2, ss, si, sd, sm, sa;

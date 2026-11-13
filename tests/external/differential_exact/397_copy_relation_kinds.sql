-- COPY opens its relation first, checks privileges and columns, and refuses a
-- view, materialized view or sequence target only once copy-in has begun.
create table ck_t (a int);
create view ck_v as select a from ck_t;
create materialized view ck_m as select a from ck_t;
create materialized view ck_e as select a from ck_t with no data;
create sequence ck_s;
create index ck_i on ck_t (a);
create table ck_p (a int) partition by list (a);
copy ck_v from stdin;
1
\.
copy ck_m from stdin;
1
\.
copy ck_s from stdin;
1
\.
copy ck_i from stdin;
\.
copy ck_v (nosuch) from stdin;
\.
copy ck_m (nosuch) from stdin;
\.
copy ck_s (nosuch) from stdin;
\.
copy ck_v (a) from stdin;
\.
copy ck_m (a) from stdin (format csv);
1
\.
copy ck_s (last_value, is_called) from stdin;
\.
copy ck_v to stdout;
copy ck_s to stdout;
copy ck_i to stdout;
copy ck_p to stdout;
copy ck_e to stdout;
copy ck_m to stdout;
create role ck_reader;
grant select (a) on ck_v to ck_reader;
set role ck_reader;
copy ck_v to stdout;
copy ck_v (a) to stdout;
copy ck_v from stdin;
\.
reset role;
-- A failed COPY's error ends it; the rest of its data is discarded.
copy ck_t from stdin;
x
2
\.
select count(*) from ck_t;
drop table ck_t, ck_p cascade;
drop sequence ck_s;
drop role ck_reader;

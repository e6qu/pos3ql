-- COPY FROM row errors name the input line, column and value in CONTEXT.
create table ct (a int not null, b text, c numeric check (c > 0), d date);
create unique index on ct (b);
copy ct from stdin;
1	x	1	2020-01-01
2	y	z	2020-01-01
\.
copy ct from stdin;
1	x	1	2020-01-01
\N	y	2	2020-01-01
\.
copy ct from stdin;
1	x	1	2020-01-01
3	y	-2	2020-01-01
\.
copy ct from stdin;
1	x	1
\.
copy ct from stdin;
1	x	1	2020-01-01	extra
\.
copy ct from stdin;
1	x	1	2020-13-45
\.
copy ct from stdin;
1	x	1	2020-01-01
2	x	1	2020-01-01
\.
copy ct from stdin (format csv);
1,x,1,2020-01-01
2,"multi
line",oops,2020-01-01
\.
copy ct from stdin (format csv, header true);
a,b,c,d
1,x,1,2020-01-01
bad,y,1,2020-01-01
\.
copy ct (a, b) from stdin;
1	aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
x	aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
\.
copy ct (a) from stdin;
99999999999
\.
copy ct (a, c) from stdin;
1	1e999999
\.
create table cs (s varchar(3));
copy cs from stdin;
abcd
\.
create table cg (a int, b int generated always as (a * 2) stored);
copy cg from stdin;
1	2
\.
copy ct from stdin with (on_error ignore);
1	x	1	2020-01-01
z	y	1	2020-01-01
\.
copy ct from stdin with (format csv, header match);
a,b,c,e
\.
copy public.ct (a) from stdin;
x
\.
select count(*) from ct;
drop table ct, cs, cg;

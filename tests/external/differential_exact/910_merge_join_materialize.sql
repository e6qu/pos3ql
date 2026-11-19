-- PostgreSQL 18 EXPLAIN ANALYZE of the Materialize PostgreSQL puts above a
-- merge join's sorted inner input: the join marks its position at each run
-- of equal keys and restores it for the next outer row of the run
-- (ExecMaterialMarkPos, ExecMaterialRestrPos), so the tuplestore trims the
-- rows before the mark and its "Maximum Storage" follows the runs, not the
-- input. The inner sorts report their spill like any tuplesort.
create table a910 (a int, b text);
insert into a910 select (g*7919)%2003, repeat('x', g%30) from generate_series(1,2000) g;
create table b910 (a int, c text);
insert into b910 select (g*104729)%2003, repeat('y', g%25) from generate_series(1,2000) g;
analyze a910; analyze b910;
set enable_hashjoin=off; set enable_nestloop=off; set work_mem='64kB';
explain (analyze, costs off, timing off, summary off, buffers off) select a910.a, a910.b, b910.c from a910 join b910 on a910.a=b910.a;
set work_mem='4MB';
explain (analyze, costs off, timing off, summary off, buffers off) select a910.a, a910.b, b910.c from a910 join b910 on a910.a=b910.a;
create table c910 (a int, c text);
insert into c910 select g%300, repeat('z', g%12) from generate_series(1,1500) g;
create table d910 (a int, d int);
insert into d910 select g%300, g from generate_series(1,900) g;
analyze c910; analyze d910;
set work_mem='4MB';
explain (analyze, costs off, timing off, summary off, buffers off) select c910.a, c910.c, d910.d from c910 join d910 on c910.a=d910.a;
explain (analyze, costs off, timing off, summary off, buffers off) select c910.a, c910.c, d910.d from c910 left join d910 on c910.a=d910.a;
explain (analyze, costs off, timing off, summary off, buffers off) select a910.a, c910.c from a910 join c910 on a910.a=c910.a;

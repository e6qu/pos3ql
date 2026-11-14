-- PL/pgSQL assigns a sub-field of a composite field: r.a.x, blk.r.a.x and
-- r.a.x.y, checking each sub-field while planning the assignment.
CREATE TYPE nf_inner AS (x int, y text);
CREATE TYPE nf_outer AS (a nf_inner, n int);
CREATE TYPE nf_deep AS (o nf_outer, k int);
DO $$ DECLARE r nf_outer; BEGIN r.a.x := 1; RAISE NOTICE '%', r; r.a.y := 'q'; RAISE NOTICE '%', r; END $$;
DO $$ <<blk>> DECLARE r nf_outer; BEGIN blk.r.a.x := 5; RAISE NOTICE '%', r; END $$;
DO $$ DECLARE d nf_deep; BEGIN d.o.a.x := 7; d.o.n := 2; RAISE NOTICE '%', d; END $$;
DO $$ DECLARE r nf_outer; BEGIN r.n.x := 1; END $$;
DO $$ DECLARE r nf_outer; BEGIN r.a.z := 1; END $$;
DO $$ DECLARE r nf_outer; BEGIN r.q.z := 1; END $$;
DO $$ DECLARE r record; BEGIN r := ROW(ROW(1,'a')::nf_inner, 2)::nf_outer; r.a.x := 9; RAISE NOTICE '%', r; END $$;
DO $$ DECLARE r record; BEGIN r.a.x := 9; END $$;
DO $$ DECLARE r nf_outer; c CONSTANT nf_outer := NULL; BEGIN c.a.x := 1; END $$;
DO $$ DECLARE r nf_outer; BEGIN r.a.x := 'zz'; END $$;
DO $$ DECLARE r nf_outer; BEGIN r.a.x.w := 1; END $$;
DO $$ DECLARE x int; BEGIN x.y.z := 1; END $$;
CREATE FUNCTION nf_set(v int) RETURNS nf_outer LANGUAGE plpgsql AS $$
DECLARE r nf_outer := ROW(ROW(0, 'keep'), 3); BEGIN r.a.x := v; RETURN r; END $$;
SELECT nf_set(4), nf_set(NULL);

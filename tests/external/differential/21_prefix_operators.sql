-- PostgreSQL's prefix arithmetic operators: |/ (square root), ||/ (cube root)
-- and @ (absolute value). They are spellings of sqrt/cbrt/abs, and sit at the
-- "any other operator" precedence level -- below binary + - * /, so the operand
-- takes the whole arithmetic expression that follows.

SELECT |/ 4;
SELECT |/ 2;
-- Cube roots are taken of exact powers of two only. `cbrt(27)` is 3 on macOS
-- and 3.0000000000000004 on the Linux CI runner -- a difference in
-- PostgreSQL's own libm, not in either engine, and cbrt (unlike sqrt) is not
-- required to be correctly rounded.
SELECT ||/ 8;
SELECT ||/ 64;
SELECT @ -3;
SELECT @ 3.5;
SELECT @ '-3'::numeric;
SELECT |/ -1;

-- they agree with the functions they name
SELECT sqrt(4), |/ 4;
SELECT cbrt(8), ||/ 8;
SELECT abs(-3), @ -3;

-- precedence: the operand swallows + - * / but not a comparison
SELECT @ -3 + 1;
SELECT |/ 4 * 2;
SELECT |/ 16 + 2;
SELECT @ -2 * 3;
SELECT ||/ 8 + 1;
SELECT 1 + @ -3;
SELECT @ -3 < 1;
SELECT @ -3 > 1;
SELECT |/ 4 = 2;

-- the operators that share their leading characters still lex correctly
SELECT 1 || '2';
SELECT 5 | 3;
SELECT 5 # 3;
SELECT '{1}'::int[] @> '{1}'::int[];
SELECT '{1}'::int[] <@ '{1}'::int[];
SELECT -3 + 1;

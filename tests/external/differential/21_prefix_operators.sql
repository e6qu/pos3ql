-- PostgreSQL's prefix arithmetic operators: |/ (square root), ||/ (cube root)
-- and @ (absolute value). They are spellings of sqrt/cbrt/abs, and sit at the
-- "any other operator" precedence level -- below binary + - * /, so the operand
-- takes the whole arithmetic expression that follows.

SELECT |/ 4;
SELECT |/ 2;
SELECT ||/ 27;
SELECT ||/ 8;
SELECT @ -3;
SELECT @ 3.5;
SELECT @ '-3'::numeric;
SELECT |/ -1;

-- they agree with the functions they name
SELECT sqrt(4), |/ 4;
SELECT cbrt(27), ||/ 27;
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

-- PREPARE parameter types keep their catalog identity: relation row types,
-- domains, enums and arrays coerce at EXECUTE with assignment rules, and
-- undeclared parameters take their types from their use.
CREATE TABLE pp_rows(a int, b text);
CREATE DOMAIN pp_positive AS int CHECK (VALUE > 0);
CREATE TYPE pp_state AS ENUM ('x', 'y');
PREPARE pp_row(pp_rows) AS SELECT $1.a, $1.b;
EXECUTE pp_row(ROW(1, 'z'));
EXECUTE pp_row('(2,w)');
EXECUTE pp_row(ROW(1, 2));
EXECUTE pp_row(ROW(1));
EXECUTE pp_row(5);
EXECUTE pp_row(ROW(1, 'z'), 2);
PREPARE pp_all(pp_rows, pp_positive, pp_state, pp_rows[]) AS SELECT $1, $2, $3, $4;
EXECUTE pp_all('(1,x)', 3, 'y', ARRAY[ROW(1, 'a')::pp_rows]);
EXECUTE pp_all('(1,x)', -3, 'y', NULL);
PREPARE pp_int(int) AS SELECT $1;
EXECUTE pp_int('x');
EXECUTE pp_int(1.5);
EXECUTE pp_int('1'::text);
EXECUTE pp_int((SELECT 4));
PREPARE pp_round AS SELECT round($1);
EXECUTE pp_round(2.5);
PREPARE pp_mixed AS SELECT $1, $2 + 1, $3 || 'x';
EXECUTE pp_mixed(1, 2, 3);
EXECUTE pp_mixed(1, 2);
SELECT name, parameter_types FROM pg_prepared_statements ORDER BY name;
PREPARE pp_missing(pp_rows) AS SELECT $1.zz;
PREPARE pp_unknown AS SELECT ($1).a;
PREPARE pp_bad(pp_no_such_type) AS SELECT 1;
DEALLOCATE ALL;
DROP TABLE pp_rows;
DROP DOMAIN pp_positive;
DROP TYPE pp_state;

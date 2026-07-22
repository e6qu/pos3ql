-- Where SQL requires a boolean, PostgreSQL requires one: AND/OR operands, a
-- NOT operand and a CASE/WHEN condition are type-checked, not treated as
-- truthy. An unknown-type literal is read as a boolean; every other type is
-- refused by name. The check happens during parse analysis, so short-circuiting
-- does not excuse an operand from it — only from being *evaluated*.

SELECT true AND 1;
SELECT 1 AND true;
SELECT 1 AND 1;
SELECT true OR 1;
SELECT 1 OR false;
SELECT true OR 1.5;
SELECT false AND 1;
SELECT true AND 1.5;
SELECT 1 AND 0;
SELECT NOT 1;
SELECT CASE WHEN 1 THEN 'a' END;

-- an unknown literal is read as a boolean, and reading it reports one that is
-- not a boolean at all -- even where the operator would short-circuit past it
SELECT true AND 'yes';
SELECT false OR 'no';
SELECT NOT 'yes';
SELECT 't' AND true;
SELECT true AND 'x';
SELECT true OR 'x';
SELECT false AND 'x';
SELECT 'x' OR true;
SELECT CASE WHEN 'yes' THEN 'a' END;

-- short-circuiting still spares an operand from a runtime error
SELECT true OR (1/0 = 1);
SELECT false AND (1/0 = 1);

-- and the ordinary boolean algebra is unchanged
SELECT true AND false;
SELECT true OR false;
SELECT true AND NULL;
SELECT NULL AND false;
SELECT NULL OR true;
SELECT NOT true;
SELECT NOT NULL;
SELECT (1=1) AND (2=2);
SELECT NOT (1=1);
SELECT CASE WHEN true THEN 'a' END;

CREATE TABLE boolarg (a int, b text);
INSERT INTO boolarg VALUES (1,'x'),(2,'y');
SELECT a FROM boolarg WHERE a;
SELECT a FROM boolarg WHERE a = 1 AND b = 'x';
SELECT a FROM boolarg WHERE a = 1 OR b = 'y' ORDER BY a;
SELECT a FROM boolarg WHERE true OR a = 1 ORDER BY a;
SELECT a FROM boolarg WHERE NOT (a = 2);
SELECT a FROM boolarg WHERE a = 1 AND 1;
DROP TABLE boolarg;

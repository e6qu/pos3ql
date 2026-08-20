-- COPY FROM WHERE evaluates the fully coerced candidate row, does not insert
-- false or NULL predicates, and works for a custom text delimiter.
DROP TABLE IF EXISTS diff_copy_where;
CREATE TABLE diff_copy_where (id integer DEFAULT 9, payload text);

COPY diff_copy_where (payload) FROM STDIN WHERE id = 9 AND payload <> 'skip';
one
skip
three
\N
\.

COPY diff_copy_where FROM STDIN WITH (DELIMITER '|') WHERE payload IS NOT NULL;
4|four
5|\N
\.

SELECT id, payload FROM diff_copy_where ORDER BY id;
DROP TABLE diff_copy_where;

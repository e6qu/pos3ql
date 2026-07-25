-- COPY TO STDOUT in CSV format, with its options. A field is quoted only when
-- it holds the delimiter, the quote, or a newline/CR, or is forced, or matches
-- the NULL string; the quote and escape characters double (or escape) inside.
-- DELIMITER, NULL, QUOTE, ESCAPE, HEADER, FORCE_QUOTE, and the legacy
-- `CSV HEADER ...` shorthand. Matches PostgreSQL 18.
--
-- The rows are loaded with INSERT rather than COPY FROM STDIN: psql feeds
-- STDIN copy data from a `-f` script line by line, and its handling of a CSV
-- data stream varies across psql builds, so COPY FROM CSV is exercised by the
-- unit tests (which drive the codec directly) instead of here.
DROP TABLE IF EXISTS cc;
CREATE TABLE cc (id int, s text, n int);
INSERT INTO cc VALUES
  (1, 'plain', 10),
  (2, 'has,comma', 20),
  (3, 'has"quote', NULL),
  (4, E'tab\tinside', 40),
  (5, NULL, 50),
  (6, '', 60),
  (7, 'NULL', 70);

-- Quote only what needs it (comma, embedded quote); NULL is the empty field.
COPY cc TO STDOUT WITH (FORMAT csv);
-- A header line of column names.
COPY cc TO STDOUT WITH (FORMAT csv, HEADER);
-- FORCE_QUOTE quotes every value, or the named columns; a non-NULL value equal
-- to the NULL string is quoted to stay distinct from NULL.
COPY cc TO STDOUT WITH (FORMAT csv, FORCE_QUOTE *);
COPY cc TO STDOUT WITH (FORMAT csv, FORCE_QUOTE (s));
COPY cc TO STDOUT WITH (FORMAT csv, NULL 'NULL');
-- A custom delimiter, NULL string, and quote character.
COPY cc TO STDOUT WITH (FORMAT csv, DELIMITER ';', NULL 'NUL', QUOTE '#');
-- A custom ESCAPE distinct from the quote character.
COPY cc TO STDOUT WITH (FORMAT csv, QUOTE '#', ESCAPE '\', FORCE_QUOTE (s));
-- The legacy shorthand.
COPY cc TO STDOUT CSV HEADER DELIMITER '|';
-- A column list reorders the output.
COPY cc (n, id) TO STDOUT WITH (FORMAT csv);

DROP TABLE cc;

-- A simple query has no parameters: a field of $n reports 42P02, as
-- PostgreSQL's parser does before resolving the field.
SELECT ($1).a;
SELECT $1.a;
SELECT $1;
SELECT ($2).b, 1;

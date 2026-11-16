-- PostgreSQL bounds JSON nesting only by its stack (`check_stack_depth`
-- under the default `max_stack_depth` of 2048kB): thousands of levels parse
-- and process, and a document nested past the stack fails with 54001
-- "stack depth limit exceeded" and its HINT. The exact boundary depends on
-- the stack each level uses, so these documents stay clear of it.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/misc/stack_depth.c
SELECT length((repeat('[', 5000) || repeat(']', 5000))::json::text);
SELECT length((repeat('[', 5000) || repeat(']', 5000))::jsonb::text);
SELECT json_typeof((repeat('[', 5000) || '1' || repeat(']', 5000))::json);
SELECT (repeat('{"a":', 5000) || '1' || repeat('}', 5000))::jsonb #> '{a,a,a}' IS NOT NULL;
SELECT (repeat('[', 5000) || repeat(']', 5000))::jsonb = (repeat('[', 5000) || repeat(']', 5000))::jsonb;
SELECT (repeat('[', 5000) || '1' || repeat(']', 5000))::jsonb @> (repeat('[', 5000) || '1' || repeat(']', 5000))::jsonb;
SELECT (repeat('[', 100000) || repeat(']', 100000))::json IS NULL;
SELECT (repeat('[', 100000) || repeat(']', 100000))::jsonb IS NULL;
SELECT (repeat('{"a":', 100000) || '1' || repeat('}', 100000))::jsonb IS NULL;
SELECT (repeat('{"a":', 100000) || '1' || repeat('}', 100000)) IS JSON;
SELECT '[[[1]]]'::jsonb -> 0 -> 0;

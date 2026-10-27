# Querying JSON values

JSON values keep their PostgreSQL types when stored, sent through a driver,
or recovered. `json` preserves the original spelling and duplicate keys;
`jsonb` normalizes objects; `jsonpath` stores a validated path expression.

**User example:** extract a typed order ID, with a stated choice for a missing
field:

```sql
SELECT JSON_VALUE('{"order_id":"12"}', '$.order_id' RETURNING integer);
SELECT JSON_VALUE('{}', '$.order_id' RETURNING integer DEFAULT 7 ON EMPTY);
```

These return integers `12` and `7`. The default is requested by the SQL
expression. Invalid paths and unsupported behavior are not silently accepted.
For multi-row documents, `JSON_TABLE` turns selected values into rows you can
join, filter, or insert like other query results.

Use the [capacity inventory](postgresql-18-compatibility.md#capacity-boundaries)
for statement and wire limits. Document support does not imply an unbounded JSON
value or a separate columnar JSON storage engine; [architecture](architecture.md)
describes the shared durable row layout.

## Supported surface

- `jsonpath` and `jsonpath[]` use PostgreSQL OIDs 4072 and 4073. Text input, binary v1 input/output, arrays, parameters, results, COPY, rows, WAL, checkpoints, and cold recovery retain that identity.
- Strict and lax execution supports root/current items, PASSING variables, members, wildcards, recursive descent, indexes, slices and `last`, filters, arithmetic, comparisons, boolean and three-valued predicates, `exists`, `starts with`, `like_regex`, conversion, numeric, structural, `keyvalue`, and datetime methods.
- `@?`, `@@`, `jsonb_path_exists`, `jsonb_path_match`, `jsonb_path_query`, `jsonb_path_query_array`, `jsonb_path_query_first`, and applicable `_tz` variants implement PostgreSQL overloads, volatility, strictness, set-returning, vars, and silent behavior.
- `JSON_EXISTS`, `JSON_VALUE`, and `JSON_QUERY` support PASSING, RETURNING, FORMAT JSON, wrappers, quote handling, and ON EMPTY/ON ERROR behaviors. Constructors, serialization, IS JSON predicates, SQL/JSON aggregates, and PostgreSQL strict/unique legacy aggregates share the same JSON conversion boundary.
- `JSON_TABLE` resolves a typed lateral row shape, including ordinality, value, FORMAT JSON, EXISTS, defaults, named and nested paths, and sibling nested columns. It is usable from queries, DML, MERGE, CTAS, COPY queries, stored views, materialized views, cursors, prepared statements, and PL/pgSQL queries.
- Record conversion covers the json/jsonb populate/to-record families, named and anonymous shapes, nested composites and arrays, domains, enums, missing/extra fields, and validity checks.
- JSONB read/write subscripting supports chained object and array paths, negative indexes, inferred containers, NULL bases, null padding, DML targets, RETURNING, triggers, PL/pgSQL locals, constraints, generated columns, rollback, WAL, and recovery.

JSON container members, path steps and subscripts, path result items, JSON_TABLE rows, and rendered text use the fixed statement arena instead of separate width or byte limits. JSON parsing still bounds nesting at 1,000 levels and path parsing at depth 128 to protect the evaluation stack. Arena exhaustion reports a program-limit error. Set-returning rows and relational operators also use their bounded materialization and spill paths.

GIN `jsonb_ops` and `jsonb_path_ops` indexes execute through object-native
posting generations with exact SQL rechecks. Native transforms,
provider-specific storage, and PostgreSQL internal heap or index formats are
not implemented. Syntax requiring those mechanisms is rejected rather than
accepted without behavior.

## Provenance and verification

Catalog identities, grammar, evaluation behavior, error SQLSTATEs, and binary format were derived from PostgreSQL 18 documentation and the `REL_18_STABLE` sources, principally:

- [JSON types](https://www.postgresql.org/docs/18/datatype-json.html)
- [JSON functions and operators](https://www.postgresql.org/docs/18/functions-json.html)
- [Aggregate functions](https://www.postgresql.org/docs/18/functions-aggregate.html)
- [`jsonpath` grammar](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/utils/adt/jsonpath_gram.y)
- [`jsonpath` input/output](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/utils/adt/jsonpath.c)
- [`jsonpath` execution](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/utils/adt/jsonpath_exec.c)
- [JSON and JSONB SQL functions](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/utils/adt/jsonfuncs.c)
- [JSONB representation utilities](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/utils/adt/jsonb_util.c)

The comparison tests are `tests/external/differential/147_sql_json.sql` and
`tests/external/differential/183_json_value_width.sql`, backed by PostgreSQL 18.4
and 18.6 respectively. They cover ordinary paths and JSON_TABLE results beyond
256 rows. Generated queries also exercise path functions, aggregates,
subscripts, and `JSON_TABLE`; wire tests check binary values and catalog types.

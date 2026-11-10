# Querying JSON values

`json`, `jsonb`, and `jsonpath` keep their PostgreSQL types through storage,
drivers, and recovery. `json` preserves the original text and duplicate keys;
`jsonb` normalizes objects; `jsonpath` stores a validated path expression.

**User example:** extract a typed order ID, choosing a default for a missing
field:

```sql
SELECT JSON_VALUE('{"order_id":"12"}', '$.order_id' RETURNING integer);           -- 12
SELECT JSON_VALUE('{}', '$.order_id' RETURNING integer DEFAULT 7 ON EMPTY);    -- 7
```

`JSON_TABLE` turns parts of a document into rows you can join, filter, or
insert. Invalid paths and unsupported syntax fail with an error.

## Supported surface

- `jsonpath` and `jsonpath[]` use PostgreSQL OIDs 4072 and 4073, with text and
  binary v1 input/output, through parameters, results, COPY, storage, and
  recovery.
- Strict and lax path execution: root/current items, `PASSING` variables,
  members, wildcards, recursive descent, indexes, slices and `last`, filters,
  arithmetic, comparisons, boolean and three-valued predicates, `exists`,
  `starts with`, `like_regex`, and conversion, numeric, structural, `keyvalue`,
  and datetime methods.
- `@?`, `@@`, `jsonb_path_exists`, `jsonb_path_match`, `jsonb_path_query`,
  `jsonb_path_query_array`, `jsonb_path_query_first`, and their `_tz` variants,
  with PostgreSQL's overloads, volatility, strictness, `vars`, and `silent`.
- `JSON_EXISTS`, `JSON_VALUE`, and `JSON_QUERY` with `PASSING`, `RETURNING`,
  `FORMAT JSON`, wrappers, quote handling, and `ON EMPTY`/`ON ERROR`.
  Constructors, serialization, `IS JSON`, SQL/JSON aggregates, and PostgreSQL's
  strict/unique aggregates.
- `JSON_TABLE` with ordinality, value, `FORMAT JSON`, `EXISTS`, defaults, named
  and nested paths, and sibling nested columns, anywhere a table source is
  allowed: queries, DML, MERGE, CTAS, COPY, views, materialized views, cursors,
  prepared statements, and PL/pgSQL.
- The `json`/`jsonb` populate and to-record families, including nested
  composites and arrays, domains, enums, and missing or extra fields.
- `jsonb` subscripting for reads and writes: chained object/array paths,
  negative indexes, inferred containers, NULL bases, null padding, and use in
  DML targets, `RETURNING`, triggers, PL/pgSQL, constraints, and generated
  columns.
- GIN `jsonb_ops` and `jsonb_path_ops` indexes, with exact rechecks.

## Limits

- JSON nesting: 1,000 levels. `jsonpath` nesting: 128.
- Everything else (members, path steps, result items, `JSON_TABLE` rows,
  rendered text) uses fixed statement memory; exhaustion returns a
  program-limit error. See [capacity boundaries](postgresql-18-compatibility.md#capacity-boundaries).
- JSON is stored in the shared row layout described in
  [architecture](architecture.md); there is no separate columnar JSON store.
  PostgreSQL transforms and native storage formats are not available, and
  syntax that needs them is rejected.

## Provenance and verification

Catalog identities, grammar, evaluation, SQLSTATEs, and binary format follow
PostgreSQL 18 documentation and `REL_18_STABLE` sources:

- [JSON types](https://www.postgresql.org/docs/18/datatype-json.html)
- [JSON functions and operators](https://www.postgresql.org/docs/18/functions-json.html)
- [Aggregate functions](https://www.postgresql.org/docs/18/functions-aggregate.html)
- [`jsonpath` grammar](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/utils/adt/jsonpath_gram.y)
- [`jsonpath` input/output](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/utils/adt/jsonpath.c)
- [`jsonpath` execution](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/utils/adt/jsonpath_exec.c)
- [JSON and JSONB SQL functions](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/utils/adt/jsonfuncs.c)
- [JSONB representation utilities](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/utils/adt/jsonb_util.c)

Differential tests `tests/external/differential/147_sql_json.sql` (PostgreSQL
18.4) and `183_json_value_width.sql` (18.6) cover paths and `JSON_TABLE`
results beyond 256 rows. Generated queries exercise path functions, aggregates,
subscripts, and `JSON_TABLE`; wire tests check binary values and catalog types.

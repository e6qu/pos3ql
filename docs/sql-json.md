# PostgreSQL 18 SQL/JSON boundary

pos3ql implements PostgreSQL 18 SQL/JSON as typed SQL and wire behavior, not as text aliases. `json` preserves lexical spelling and duplicate keys; `jsonb` uses canonical object semantics; `jsonpath` is parsed and validated at every input boundary and stored in canonical text form.

## Supported surface

- `jsonpath` and `jsonpath[]` use PostgreSQL OIDs 4072 and 4073. Text input, binary v1 input/output, arrays, parameters, results, COPY, rows, WAL, checkpoints, and cold recovery retain that identity.
- Strict and lax execution supports root/current items, PASSING variables, members, wildcards, recursive descent, indexes, slices and `last`, filters, arithmetic, comparisons, boolean and three-valued predicates, `exists`, `starts with`, `like_regex`, conversion, numeric, structural, `keyvalue`, and datetime methods.
- `@?`, `@@`, `jsonb_path_exists`, `jsonb_path_match`, `jsonb_path_query`, `jsonb_path_query_array`, `jsonb_path_query_first`, and applicable `_tz` variants implement PostgreSQL overloads, volatility, strictness, set-returning, vars, and silent behavior.
- `JSON_EXISTS`, `JSON_VALUE`, and `JSON_QUERY` support PASSING, RETURNING, FORMAT JSON, wrappers, quote handling, and ON EMPTY/ON ERROR behaviors. Constructors, serialization, IS JSON predicates, SQL/JSON aggregates, and PostgreSQL strict/unique legacy aggregates share the same JSON conversion boundary.
- `JSON_TABLE` resolves a typed lateral row shape, including ordinality, value, FORMAT JSON, EXISTS, defaults, named and nested paths, and sibling nested columns. It is usable from queries, DML, MERGE, CTAS, COPY queries, stored views, materialized views, cursors, prepared statements, and PL/pgSQL queries.
- Record conversion covers the json/jsonb populate/to-record families, named and anonymous shapes, nested composites and arrays, domains, enums, missing/extra fields, and validity checks.
- JSONB read/write subscripting supports chained object and array paths, negative indexes, inferred containers, NULL bases, null padding, DML targets, RETURNING, triggers, PL/pgSQL locals, constraints, generated columns, rollback, WAL, and recovery.

Path parsing is bounded to 256 steps and subscripts, depth 128, and 65,536 canonical bytes. One execution produces at most 1,024 path items. These are startup-bounded statement-arena limits; overflow is a named program-limit error. Set-returning rows and relational operators additionally use the ordinary bounded materialization and spill paths.

GIN/GiST execution, native transforms, provider-specific storage, and PostgreSQL internal heap or index formats are not implemented. Syntax requiring those mechanisms is rejected rather than accepted without behavior.

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

The conformance ratchet is `tests/external/differential/147_sql_json.sql`, backed by PostgreSQL 18.4. The generated differential fuzzer also emits path operators/functions, aggregates, subscripts, lateral set-returning queries, and `JSON_TABLE`; raw-wire and type-fidelity probes cover binary and catalog metadata.

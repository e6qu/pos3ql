# PostgreSQL 18 SQL/XML boundary

pos3ql models PostgreSQL's `xml` and `xml[]` types as validated UTF-8 XML
spelling. The original value is retained for casts to `text`; XML text and
binary output apply PostgreSQL's declaration rules, including removal of an
encoding declaration and omission of a redundant XML 1.0 declaration.

The type boundary includes PostgreSQL catalog OIDs, arrays, text and binary
Bind/Result, portals, COPY, table storage, generated expressions and checks,
WAL, checkpoints, and object-store cold recovery. `xmloption` is a
transactional session setting: `content` accepts fragments and `document`
requires exactly one document element.

## SQL/XML expressions

The supported SQL syntax is:

- `XMLPARSE`, `XMLSERIALIZE`, `XMLELEMENT` with `XMLATTRIBUTES`, `XMLFOREST`,
  `XMLCONCAT`, `XMLCOMMENT`, `XMLPI`, `XMLROOT`, and `XMLEXISTS`;
- `xml_is_well_formed`, `xml_is_well_formed_document`, and
  `xml_is_well_formed_content`;
- `xpath` and `xpath_exists`, including PostgreSQL's two-dimensional namespace
  mapping array;
- ordered `XMLAGG` with PostgreSQL NULL behavior; and
- typed `XMLTABLE`, including namespace declarations, ordinality, `PATH`,
  `DEFAULT`, `NULL`/`NOT NULL`, XML-valued multi-node columns, and scalar
  cardinality checks.

`XMLTABLE` is an ordinary lateral query source. It participates in joins,
INSERT/UPDATE/DELETE/MERGE sources, CTAS, COPY, views, materialized views,
cursors, prepared statements, and PL/pgSQL. Stored definitions and values
survive WAL replay, checkpoints, and cacheless object-store recovery.

## XPath and resource limits

The allocation-free XPath evaluator supports child and descendant paths
(`/` and `//`), named and wildcard elements, attributes, `text()`, numeric
positions, attribute-equality predicates, and the scalar `count()`,
`boolean()`, `string()`, and `number()` forms. Namespace mappings match by URI,
so the query prefix need not equal the source document's prefix. Other XPath
axes, operators, and functions are outside the executable subset and reject
rather than being ignored.

Runtime work is bounded at startup. One XML value may nest 64 elements; an
element may have 128 attributes; a document may declare 128 entities; an
XPath evaluation may index 1,024 elements and 4,096 attributes and use 64 path
steps. Constructors and XPath scalar materialization are limited to 64 KiB.
Crossing a limit returns a named program-limit error and never allocates a
growing runtime structure.

External entity retrieval and native XML/XSLT libraries are not part of the
runtime. This preserves the `libc`-only core and prevents XML evaluation from
opening an unbudgeted I/O path. PostgreSQL's unsupported `DEFAULT` namespace
spelling in `XMLTABLE` is rejected explicitly as well.

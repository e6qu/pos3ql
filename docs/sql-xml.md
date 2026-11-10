# Querying XML values

**User example:** read order IDs and names from an XML document:

```sql
SELECT * FROM XMLTABLE(
  '/orders/order' PASSING
    ('<orders><order id="1"><name>Gift</name></order></orders>'::xml)
  COLUMNS id integer PATH '@id', name text PATH 'name');
```

This returns one row: `id = 1`, `name = 'Gift'`. pos3ql supports a subset of
XPath (the query language inside `PATH`); expressions outside it fail with an
error.

`xml` and `xml[]` store validated UTF-8 text with PostgreSQL's catalog OIDs,
through text and binary values, COPY, storage, and recovery. Casting to `text`
returns the original value; XML output follows PostgreSQL's declaration rules
(the encoding declaration is removed and a redundant XML 1.0 declaration is
omitted). The transactional `xmloption` setting chooses `content` (fragments
allowed) or `document` (exactly one root element).

## SQL/XML expressions

- `XMLPARSE`, `XMLSERIALIZE`, `XMLELEMENT` with `XMLATTRIBUTES`, `XMLFOREST`,
  `XMLCONCAT`, `XMLCOMMENT`, `XMLPI`, `XMLROOT`, and `XMLEXISTS`;
- `xml_is_well_formed`, `xml_is_well_formed_document`, and
  `xml_is_well_formed_content`;
- `xpath` and `xpath_exists`, including the two-dimensional namespace array;
- ordered `XMLAGG` with PostgreSQL NULL behavior;
- `XMLTABLE` with namespaces, ordinality, `PATH`, `DEFAULT`, `NULL`/`NOT NULL`,
  multi-node XML columns, and scalar cardinality checks. It works anywhere a
  table source is allowed: joins, DML and MERGE sources, CTAS, COPY, views,
  materialized views, cursors, prepared statements, and PL/pgSQL.

As in [PostgreSQL 18's grammar](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/parser/gram.y),
the `PASSING` operand is a primary expression (parenthesize a cast on a compound
expression), and `BY REF`/`BY VALUE` may appear before or after it. The
`DEFAULT` namespace spelling in `XMLTABLE`, which PostgreSQL also does not
support, is rejected.

## XPath subset

Supported: child and descendant paths (`/`, `//`), named and wildcard elements,
attributes, `text()`, numeric positions, attribute-equality predicates, and
scalar `count()`, `boolean()`, `string()`, and `number()`. Namespace prefixes
match by URI, so the query prefix need not equal the document's. Other axes,
operators, and functions are rejected.

## Limits

| Limit | Value |
|---|---|
| Element nesting per value | 64 |
| Attributes per element | 128 |
| Entity declarations per document | 128 |
| Elements / attributes indexed per XPath evaluation | 1,024 / 4,096 |
| Path steps per XPath expression | 64 |
| Constructor and XPath scalar output | 64 KiB |
| `XMLTABLE` rows | Fixed statement memory |

Exceeding a limit returns a named program-limit error. External entities are
never fetched, and no native XML or XSLT library is used. Statement, wire, and
row sizes are listed under
[capacity boundaries](postgresql-18-compatibility.md#capacity-boundaries).

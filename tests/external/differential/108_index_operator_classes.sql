-- Every stored type with PostgreSQL btree semantics selects the same default
-- operator class, including polymorphic user-defined families.
DROP TABLE IF EXISTS index_operator_rows;
DROP TYPE IF EXISTS index_operator_pair;
DROP TYPE IF EXISTS index_operator_mood;

CREATE TYPE index_operator_mood AS ENUM ('low', 'high');
CREATE TYPE index_operator_pair AS (left_value integer, right_value text);
CREATE TABLE index_operator_rows (
    bits bit(3), varying_bits varbit(3), fixed_text char(3), named name,
    occurred_at timestamp, occurred_at_tz timestamptz, clock_time time,
    clock_time_tz timetz, address inet, network cidr, hardware macaddr,
    hardware8 macaddr8, identity uuid, object_id oid, values integer[],
    span int4range, spans int4multirange, mood index_operator_mood,
    pair index_operator_pair
);
CREATE INDEX index_operator_scalar ON index_operator_rows
    (bits, varying_bits, fixed_text, named, occurred_at, occurred_at_tz, clock_time, clock_time_tz);
CREATE INDEX index_operator_network ON index_operator_rows
    (address, network, hardware, hardware8, identity, object_id);
CREATE INDEX index_operator_polymorphic ON index_operator_rows
    (values, span, spans, mood, pair);

SELECT relation.relname, index.indclass
FROM pg_index index
JOIN pg_class relation ON relation.oid = index.indexrelid
WHERE relation.relname LIKE 'index_operator_%'
ORDER BY relation.relname;

CREATE INDEX index_operator_explicit ON index_operator_rows
    (pair record_ops, values array_ops, mood enum_ops);
SELECT pg_get_indexdef('index_operator_explicit'::regclass);

CREATE INDEX index_operator_bad_name ON index_operator_rows (pair missing_ops);
CREATE INDEX index_operator_bad_type ON index_operator_rows (pair text_ops);

DROP TABLE index_operator_rows;
DROP TYPE index_operator_pair;
DROP TYPE index_operator_mood;

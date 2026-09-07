// Differential driver test for node-postgres (pg) against pos3ql and real
// PostgreSQL. Prints a deterministic transcript (parameterized CRUD,
// transaction rollback, catalog introspection) so the CI harness can diff
// pos3ql's output against PostgreSQL's.
//
// Usage:  node node_test.js <host> <port>
'use strict';
const { Client } = require('pg');

const host = process.argv[2] || '127.0.0.1';
const port = parseInt(process.argv[3] || '5432', 10);
const out = [];
const line = (s) => out.push(s);

async function main() {
  const c = new Client({ host, port, user: 'postgres', database: 'postgres', ssl: false });
  await c.connect();
  try {
    await c.query('DROP TABLE IF EXISTS node_drv');
    await c.query('DROP TABLE IF EXISTS node_types');
    await c.query('DROP TABLE IF EXISTS node_sample_source');
    await c.query('CREATE TABLE node_drv (id int PRIMARY KEY, name text, score float8)');

    // Parameterized inserts (extended protocol).
    for (const r of [[1, 'ada', 9.5], [2, 'bob', 7.25], [3, 'cyd', null]]) {
      const res = await c.query('INSERT INTO node_drv VALUES ($1,$2,$3)', r);
      line('insert rows=' + res.rowCount);
    }

    const sel = await c.query(
      'SELECT id, name, score FROM node_drv WHERE id <= $1 ORDER BY id', [2]);
    for (const row of sel.rows) {
      line(`row ${row.id}|${row.name}|${row.score}`);
    }

    line('update rows=' + (await c.query('UPDATE node_drv SET score=$1 WHERE id=$2', [10, 3])).rowCount);
    line('delete rows=' + (await c.query('DELETE FROM node_drv WHERE id=$1', [2])).rowCount);

    await c.query('CREATE TABLE node_types (id int PRIMARY KEY, label varchar(4), amount numeric(7,2), ' +
      'ids integer[], note jsonb, span int4range, key uuid)');
    line('typed insert rows=' + (await c.query(
      'INSERT INTO node_types VALUES ($1,$2,$3,$4,$5,$6,$7)',
      [1, 'wide', '12.345', [1, null, 3], { ready: true }, '[1,5)',
        '00112233-4455-6677-8899-aabbccddeeff'])).rowCount);
    const typed = (await c.query(
      'SELECT label, amount, ids, note, span::text, key::text FROM node_types')).rows[0];
    line(`typed ${typed.label}|${typed.amount}|${JSON.stringify(typed.ids)}|` +
      `${JSON.stringify(typed.note)}|${typed.span}|${typed.key}`);

    await c.query("SET application_name TO 'outside'");
    await c.query("DROP TABLE IF EXISTS node_routine_log");
    await c.query("CREATE TABLE node_routine_log(value integer)");
    await c.query("CREATE OR REPLACE FUNCTION node_standard(value integer) RETURNS integer " +
      "LANGUAGE SQL IMMUTABLE STRICT SET application_name TO 'driver' RETURN value + 1");
    await c.query("CREATE OR REPLACE PROCEDURE node_standard_proc(value integer) " +
      "LANGUAGE SQL AS 'INSERT INTO node_routine_log VALUES (value)'");
    const routine = await c.query(
      "SELECT node_standard($1) AS value, current_setting('application_name') AS application_name",
      [41]);
    line(`standard routine ${routine.rows[0].value}|${routine.rows[0].application_name}`);
    await c.query("CALL node_standard_proc($1)", [43]);
    const procedure = await c.query("SELECT value FROM node_routine_log");
    line(`standard procedure ${procedure.rows[0].value}`);
    await c.query("CREATE OR REPLACE PROCEDURE node_plpgsql(value integer) LANGUAGE plpgsql " +
      "AS 'BEGIN INSERT INTO node_routine_log VALUES (value + 1); END'");
    await c.query("CALL node_plpgsql($1)", [44]);
    const plpgsql = await c.query("SELECT value FROM node_routine_log WHERE value = 45");
    line(`plpgsql procedure ${plpgsql.rows[0].value}`);
    await c.query("CREATE OR REPLACE FUNCTION node_dynamic_analyze(value integer) RETURNS boolean " +
      "LANGUAGE plpgsql AS 'DECLARE plan text; BEGIN " +
      "EXECUTE ''EXPLAIN (ANALYZE, FORMAT JSON) SELECT $1 + 1'' " +
      "INTO STRICT plan USING value; RETURN plan IS NOT NULL; END'");
    const dynamicAnalyze = await c.query('SELECT node_dynamic_analyze($1) AS ok', [41]);
    line(`plpgsql dynamic analyze ${dynamicAnalyze.rows[0].ok}`);
    const arraySubscripts = await c.query(
      "SELECT array_position('[4:6]={10,20,10}'::integer[], 10) AS first, " +
      "array_positions('[4:6]={10,20,10}'::integer[], 10)::text AS positions, " +
      "array_remove('[4:6]={10,20,10}'::integer[], 10)::text AS removed");
    line(`array subscripts ${arraySubscripts.rows[0].first}|${arraySubscripts.rows[0].positions}|${arraySubscripts.rows[0].removed}`);

    // Transaction rollback must not persist.
    await c.query('BEGIN');
    await c.query("INSERT INTO node_drv VALUES (9,'tmp',null)");
    await c.query('ROLLBACK');
    const n = await c.query('SELECT count(*)::int AS n FROM node_drv WHERE id=9');
    line('after rollback id=9 count=' + n.rows[0].n);

    await c.query('DROP TABLE IF EXISTS node_two_phase');
    await c.query('CREATE TABLE node_two_phase (id integer PRIMARY KEY, value text)');
    await c.query("INSERT INTO node_two_phase VALUES (1, 'before')");
    await c.query('BEGIN');
    await c.query('UPDATE node_two_phase SET value = $1 WHERE id = $2', ['after', 1]);
    await c.query("PREPARE TRANSACTION 'node-two-phase'");
    const prepared = await c.query(
      'SELECT gid, pg_typeof(transaction)::text AS type FROM pg_prepared_xacts');
    line(`two-phase prepared ${prepared.rows[0].gid}|${prepared.rows[0].type}`);
    const hidden = await c.query('SELECT value FROM node_two_phase WHERE id = 1');
    line('two-phase hidden ' + hidden.rows[0].value);
    await c.query("COMMIT PREPARED 'node-two-phase'");
    const committed = await c.query('SELECT value FROM node_two_phase WHERE id = 1');
    line('two-phase committed ' + committed.rows[0].value);

    // Catalog introspection (the `'tbl'::regclass` pattern).
    const cols = await c.query(
      "SELECT attname, format_type(atttypid, atttypmod) AS t, attnotnull " +
      "FROM pg_attribute WHERE attrelid = 'node_drv'::regclass AND attnum > 0 " +
      "AND NOT attisdropped ORDER BY attnum");
    for (const r of cols.rows) {
      line(`col ${r.attname}|${r.t}|notnull=${r.attnotnull}`);
    }
    const typedCols = await c.query(
      "SELECT attname, format_type(atttypid, atttypmod) AS t " +
      "FROM pg_attribute WHERE attrelid = 'node_types'::regclass AND attnum > 0 " +
      "AND NOT attisdropped ORDER BY attnum");
    for (const r of typedCols.rows) line(`typed-col ${r.attname}|${r.t}`);

    await c.query('CREATE TABLE node_sample_source (id integer PRIMARY KEY)');
    await c.query('INSERT INTO node_sample_source SELECT value FROM generate_series(1,20) value');
    const sampled = await c.query(
      'SELECT count(*)::int AS n FROM node_sample_source ' +
      'TABLESAMPLE BERNOULLI ($1) REPEATABLE ($2)', [100.0, 42.0]);
    line('sample rows=' + sampled.rows[0].n);
    const emptySample = await c.query(
      'SELECT count(*)::int AS n FROM node_sample_source ' +
      'TABLESAMPLE SYSTEM ($1) REPEATABLE ($2)', [0.0, 42.0]);
    line('system sample rows=' + emptySample.rows[0].n);

    await c.query('DROP TABLE IF EXISTS node_persistence');
    await c.query('DROP TABLE IF EXISTS node_unlogged');
    await c.query('CREATE TABLE node_persistence (id integer)');
    await c.query('INSERT INTO node_persistence VALUES (1)');
    await c.query('CREATE UNLOGGED TABLE node_unlogged (id integer)');
    await c.query('INSERT INTO node_unlogged VALUES ($1)', [7]);
    await c.query('CREATE TEMP TABLE node_persistence (id integer) ON COMMIT PRESERVE ROWS');
    await c.query('INSERT INTO node_persistence VALUES ($1), ($2)', [2, 3]);
    const persistence = await c.query(
      "SELECT (SELECT relpersistence FROM pg_class WHERE oid='node_persistence'::regclass) AS temp, " +
      "(SELECT relpersistence FROM pg_class WHERE oid='public.node_persistence'::regclass) AS permanent, " +
      "(SELECT relpersistence FROM pg_class WHERE oid='node_unlogged'::regclass) AS unlogged");
    line(`persistence ${persistence.rows[0].temp}|${persistence.rows[0].permanent}|${persistence.rows[0].unlogged}`);
    const other = new Client({ host, port, user: 'postgres', database: 'postgres', ssl: false });
    await other.connect();
    try {
      const ownerRows = await c.query('SELECT id FROM node_persistence ORDER BY id');
      const otherRows = await other.query('SELECT id FROM node_persistence ORDER BY id');
      line(`temp isolation ${ownerRows.rows.map(r => r.id).join(',')}|${otherRows.rows.map(r => r.id).join(',')}`);
      const otherTemp = await other.query(
        "SELECT pg_my_temp_schema() AS oid, to_regclass('pg_temp.node_persistence') IS NULL AS missing");
      line(`other temp ${otherTemp.rows[0].oid}|${otherTemp.rows[0].missing}`);
    } finally {
      await other.end();
    }
    await c.query('BEGIN');
    await c.query('CREATE TEMP TABLE node_delete (id integer) ON COMMIT DELETE ROWS');
    await c.query('CREATE TEMP TABLE node_drop (id integer) ON COMMIT DROP');
    await c.query('INSERT INTO node_delete VALUES ($1)', [9]);
    await c.query('INSERT INTO node_drop VALUES ($1)', [10]);
    await c.query('COMMIT');
    const commitState = await c.query(
      "SELECT (SELECT count(*)::int FROM node_delete) AS deleted, to_regclass('node_drop') IS NULL AS dropped");
    line(`on commit ${commitState.rows[0].deleted}|${commitState.rows[0].dropped}`);
  } finally {
    await c.end();
  }
  process.stdout.write(out.join('\n') + '\n');
}

main().catch((e) => { console.log('FATAL', e.message); process.exit(1); });

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

    // Transaction rollback must not persist.
    await c.query('BEGIN');
    await c.query("INSERT INTO node_drv VALUES (9,'tmp',null)");
    await c.query('ROLLBACK');
    const n = await c.query('SELECT count(*)::int AS n FROM node_drv WHERE id=9');
    line('after rollback id=9 count=' + n.rows[0].n);

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
  } finally {
    await c.end();
  }
  process.stdout.write(out.join('\n') + '\n');
}

main().catch((e) => { console.log('FATAL', e.message); process.exit(1); });

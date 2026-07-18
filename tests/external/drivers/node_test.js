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
  } catch (e) {
    line('FATAL ' + (e.code || '?????') + ' ' + String(e.message).split('\n')[0]);
  } finally {
    await c.end();
  }
  process.stdout.write(out.join('\n') + '\n');
}

main().catch((e) => { console.log('FATAL', e.message); process.exit(1); });

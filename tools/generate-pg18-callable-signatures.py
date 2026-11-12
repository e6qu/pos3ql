#!/usr/bin/env python3
"""Add PostgreSQL 18.6 callable contracts to src/sql/catalog_callable_signatures.rs.

usage: generate-pg18-callable-signatures.py --dsn DSN RS_FILE [OID...]

Extends the recorded extraction SQL's OID list with OID..., reruns the
extraction against a PostgreSQL 18.6 reference server, refreshes both recorded
SHA-256 digests (SQL text plus newline; `psql -At` output), inserts the
requested entries in (name, oid) order and rebuilds CALLABLE_OIDS. It also
rewrites CALLABLE_TYPE_NAMES, PostgreSQL's format_type name of every type a
pg_proc row uses, and CALLABLE_INITIAL_ACLS, the initdb proacl of every
routine that has one (read from template1, which no session changes), each
with its own SQL and result digests. With no OIDs it only re-verifies the
recorded digests and index.
"""
import argparse, hashlib, json, re, subprocess, sys
parser = argparse.ArgumentParser()
parser.add_argument("--dsn", required=True, help="libpq connection string of a PostgreSQL 18.6 server")
parser.add_argument("path")
parser.add_argument("oids", nargs="*", type=int)
options = parser.parse_args()
path, oids = options.path, options.oids
version = subprocess.run(["psql", "-X", "-At", options.dsn, "-c", "SHOW server_version"],
                         capture_output=True, text=True, check=True).stdout.strip()
if not version.startswith("18.6"):
    sys.exit(f"reference server is {version}, not PostgreSQL 18.6")
s = open(path).read()
a = s.index('/* Exact extraction SQL:\n') + len('/* Exact extraction SQL:\n')
b = s.index('\n*/', a)
sql = s[a:b]
m = re.search(r'WHERE p\.oid IN \(([0-9,]+)\)', sql)
listed = [int(x) for x in m.group(1).split(',')]
listed = sorted(set(listed) | set(oids))
sql = sql[:m.start(1)] + ','.join(map(str, listed)) + sql[m.end(1):]
out = subprocess.run(['psql', '-X', '-At', options.dsn, '-c', sql], capture_output=True, text=True, check=True).stdout
s = s[:a] + sql + s[b:]
s = re.sub(r'Extraction SQL SHA256: [0-9a-f]+', 'Extraction SQL SHA256: ' + hashlib.sha256((sql + '\n').encode()).hexdigest(), s)
s = re.sub(r'Result JSON SHA256: [0-9a-f]+', 'Result JSON SHA256: ' + hashlib.sha256(out.encode()).hexdigest(), s)
rows = {e['oid']: e for e in json.loads(out)}
def rs(v): return json.dumps(v, ensure_ascii=False)
def arr(xs): return '&[' + ', '.join(xs) + ']'
def fl(x):
    r = repr(float(x))
    return r if 'e' not in r else '%se%s' % tuple(r.split('e'))
def entry(e):
    args = [x for x in e['arguments'].split(' ') if x]
    names = e['names'] or []
    modes = e['modes'] or []
    nin = len(args)
    if modes:
        innames = [n for n, md in zip(names, modes) if md in ('i', 'b', 'v')]
    else:
        innames = names
    innames = innames if any(innames) else []
    var = e['variadic']
    lines = [
        '    BuiltinCallableSignature::new(',
        f"        {e['oid']},", f"        {rs(e['schema'])},", f"        {rs(e['name'])},",
        f"        {arr(args)},", f"        {arr([rs(n) for n in innames])},",
        f"        {nin - e['defaults']},",
        f"        {'Some(' + rs(e['default_sql']) + ')' if e['default_sql'] else 'None'},",
        f"        {'Some(%d)' % var if var else 'None'},",
        f"        b'{e['kind']}',", f"        {'true' if e['set'] else 'false'},", f"        {e['result']},",
        '        BuiltinCallableCatalog {',
        f"            volatility: {rs(e['volatility'])},", f"            parallel: {rs(e['parallel'])},",
        f"            strict: {'true' if e['strict'] else 'false'},", f"            leakproof: {'true' if e['leakproof'] else 'false'},",
        f"            security_definer: {'true' if e['security_definer'] else 'false'},", f"            language: {e['language']},",
        f"            source: {rs(e['source'])},", f"            cost: {fl(e['cost'])},", f"            rows: {fl(e['rows'])},",
        f"            support: {e['support']},", f"            support_name: {rs(e['support_name'])},",
        f"            all_arguments: {arr([str(x) for x in (e['all_types'] or [])])},",
        f"            modes: {arr([str(ord(x)) for x in modes])},",
        f"            all_argument_names: {arr([rs(n) for n in (names if e['all_types'] else names)])},",
        '        },', '    ),']
    return '\n'.join(lines) + '\n'
start = s.index('pub(super) const CALLABLES: &[BuiltinCallableSignature] = &[\n') + len('pub(super) const CALLABLES: &[BuiltinCallableSignature] = &[\n')
for oid in oids:
    e = rows[oid]
    if re.search(r'BuiltinCallableSignature::new\(\n        %d,\n' % oid, s):
        continue
    key = (e['name'], oid)
    pos = start
    for m in re.finditer(r'    BuiltinCallableSignature::new\(\n        (\d+),\n        "[^"]*",\n        "([^"]*)",', s[start:]):
        if (m.group(2), int(m.group(1))) > key:
            pos = start + m.start(); break
    else:
        pos = s.index('\n];', start) + 1
    s = s[:pos] + entry(e) + s[pos:]
# Rebuild the OID index over the final CALLABLES order.
body_start = s.index('pub(super) const CALLABLES: &[BuiltinCallableSignature] = &[\n')
body_end = s.index('\n];', body_start)
order = [int(m.group(1)) for m in re.finditer(r'    BuiltinCallableSignature::new\(\n        (\d+),\n', s[body_start:body_end])]
index = sorted((oid, at) for at, oid in enumerate(order))
o_start = s.index('pub(super) const CALLABLE_OIDS: &[(i32, usize)] = &[\n') + len('pub(super) const CALLABLE_OIDS: &[(i32, usize)] = &[\n')
o_end = s.index('];', o_start)
s = s[:o_start] + ''.join('    (%d, %d),\n' % pair for pair in index) + s[o_end:]
# Every type a pg_proc row names, as format_type_be prints it.
TYPE_SQL = ("SELECT jsonb_agg(jsonb_build_array(t.oid::integer, format_type(t.oid, NULL)) ORDER BY t.oid) "
            "FROM pg_type t WHERE t.oid IN (SELECT unnest(p.proargtypes::oid[] || "
            "coalesce(p.proallargtypes, '{}') || p.prorettype) FROM pg_proc p)")
type_out = subprocess.run(['psql', '-X', '-At', options.dsn, '-c', TYPE_SQL], capture_output=True, text=True, check=True).stdout
type_rows = json.loads(type_out)
block = ('/* Exact type-name extraction SQL:\n' + TYPE_SQL + '\n*/\n'
         '// Type-name SQL SHA256: ' + hashlib.sha256((TYPE_SQL + '\n').encode()).hexdigest() + '\n'
         '// Type-name result JSON SHA256: ' + hashlib.sha256(type_out.encode()).hexdigest() + '\n'
         '/// `format_type` names of the types `pg_proc` rows use, sorted by OID.\n'
         'pub(super) const CALLABLE_TYPE_NAMES: &[(i32, &str)] = &[\n'
         + ''.join('    (%d, %s),\n' % (oid, rs(name)) for oid, name in type_rows) + '];\n')
# initdb's proacl, which pg_init_privs records, from the unmodified template.
ACL_SQL = ("SELECT jsonb_agg(jsonb_build_array(p.oid::integer, p.proacl::text[]) ORDER BY p.oid) "
           "FROM pg_proc p WHERE p.proacl IS NOT NULL")
template = options.dsn + ' dbname=template1'
acl_out = subprocess.run(['psql', '-X', '-At', template, '-c', ACL_SQL], capture_output=True, text=True, check=True).stdout
block += ('/* Exact initial-ACL extraction SQL (database template1):\n' + ACL_SQL + '\n*/\n'
          '// Initial-ACL SQL SHA256: ' + hashlib.sha256((ACL_SQL + '\n').encode()).hexdigest() + '\n'
          '// Initial-ACL result JSON SHA256: ' + hashlib.sha256(acl_out.encode()).hexdigest() + '\n'
          '/// The `proacl` initdb gives a routine, sorted by OID; every other routine\n'
          '/// keeps the default ACL (owner and `PUBLIC` may execute).\n'
          'pub(super) const CALLABLE_INITIAL_ACLS: &[(i32, &[&str])] = &[\n'
          + ''.join('    (%d, %s),\n' % (oid, arr([rs(item) for item in items])) for oid, items in json.loads(acl_out)) + '];\n')
marker = '/* Exact type-name extraction SQL:\n'
if marker in s:
    s = s[:s.index(marker)] + block
else:
    s = s.rstrip('\n') + '\n\n' + block
open(path, 'w').write(s)

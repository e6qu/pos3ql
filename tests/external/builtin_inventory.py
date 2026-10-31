"""Inventory PostgreSQL built-in signatures; catalog presence is not execution proof."""
import argparse
import hashlib
import json
from pathlib import Path

import psycopg
from psycopg.rows import dict_row


QUERY = """
SELECT oid::bigint AS oid,proname,prokind,prorettype::bigint AS prorettype,
       proargtypes::text AS proargtypes,pronargs,proretset,proisstrict,provolatile
FROM pg_catalog.pg_proc WHERE pronamespace=11 ORDER BY oid
"""


def capture(host, port, user, database):
    with psycopg.connect(host=host, port=port, user=user, dbname=database,
                         sslmode="disable", autocommit=True, connect_timeout=5,
                         row_factory=dict_row) as connection:
        version = connection.execute("SELECT version() AS version").fetchone()["version"]
        return {"version": version, "routines": connection.execute(QUERY).fetchall()}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--reference-host", required=True)
    parser.add_argument("--reference-port", required=True, type=int)
    parser.add_argument("--candidate-host", required=True)
    parser.add_argument("--candidate-port", required=True, type=int)
    parser.add_argument("--user", required=True)
    parser.add_argument("--database", required=True)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    reference = capture(args.reference_host, args.reference_port, args.user, args.database)
    candidate = capture(args.candidate_host, args.candidate_port, args.user, args.database)
    expected = {row["oid"]: row for row in reference["routines"]}
    actual = {row["oid"]: row for row in candidate["routines"]}
    if len(expected) != len(reference["routines"]) or len(actual) != len(candidate["routines"]):
        raise AssertionError("duplicate routine OID in catalog inventory")
    missing = [expected[oid] for oid in sorted(expected.keys() - actual.keys())]
    differences = [{"reference": expected[oid], "candidate": actual[oid]}
                   for oid in sorted(expected.keys() & actual.keys()) if expected[oid] != actual[oid]]
    report = {
        "revision": args.revision, "query": QUERY.strip(),
        "reference": reference, "candidate": candidate,
        "missing_catalog_routines": missing,
        "catalog_metadata_differences": differences,
        "catalog_matches_reference": not missing and not differences,
        "execution_coverage": "Not established by this catalog inventory; use executable comparison fixtures.",
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    checksum = hashlib.sha256(args.output.read_bytes()).hexdigest()
    print(f"Built-in catalog inventory: reference={len(expected)}, candidate={len(actual)}, "
          f"missing={len(missing)}, metadata differences={len(differences)}; SHA256={checksum}")
    if differences:
        raise SystemExit("Registered built-in signatures or properties differ from PostgreSQL; see the retained inventory")


if __name__ == "__main__":
    main()

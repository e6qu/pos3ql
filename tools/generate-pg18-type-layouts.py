#!/usr/bin/env python3
"""Render physical type properties captured from the pinned PostgreSQL service."""

import argparse
import hashlib
import json
from pathlib import Path


def render(metadata: dict, source: str) -> str:
    if not metadata["version"].startswith("PostgreSQL 18.6 "):
        raise ValueError("type layouts require PostgreSQL 18.6")
    rows = metadata["type_layouts"]
    if not rows:
        raise ValueError("physical type properties must not be empty")
    previous = 0
    entries = []
    for oid, name, length, by_value, alignment, storage in rows:
        if (
            type(oid) is not int
            or oid <= previous
            or not isinstance(name, str)
            or type(length) is not int
            or not -32768 <= length <= 32767
            or not isinstance(by_value, bool)
            or alignment not in ("c", "s", "i", "d")
            or storage not in ("p", "e", "x", "m")
        ):
            raise ValueError(f"invalid physical type properties: {oid!r}")
        previous = oid
        entries.append(
            f"    ({oid}, PhysicalLayout {{ length: {length}, by_value: "
            f"{str(by_value).lower()}, alignment: b'{alignment}', storage: b'{storage}' }}),"
        )
    digest = hashlib.sha256(
        json.dumps(rows, separators=(",", ":")).encode()
    ).hexdigest()
    return (
        "//! Physical type properties from PostgreSQL 18.6.\n"
        f"//! Source: {source}\n"
        f"//! Captured rows SHA-256: {digest}\n"
        "//! PostgreSQL license: vendor/test/postgres-regress/COPYRIGHT\n"
        "//! Regenerate with tools/generate-pg18-type-layouts.py.\n\n"
        "#[derive(Clone, Copy, Debug, PartialEq, Eq)]\n"
        "pub(crate) struct PhysicalLayout {\n"
        "    pub(crate) length: i16,\n"
        "    pub(crate) by_value: bool,\n"
        "    pub(crate) alignment: u8,\n"
        "    pub(crate) storage: u8,\n"
        "}\n\n"
        "#[rustfmt::skip]\n"
        "const LAYOUTS: &[(i32, PhysicalLayout)] = &[\n"
        + "\n".join(entries)
        + "\n];\n\n"
        "pub(crate) fn builtin(oid: i32) -> Option<PhysicalLayout> {\n"
        "    LAYOUTS\n"
        "        .binary_search_by_key(&oid, |(oid, _)| *oid)\n"
        "        .ok()\n"
        "        .map(|position| LAYOUTS[position].1)\n"
        "}\n"
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--metadata", type=Path, required=True)
    parser.add_argument("--source", required=True)
    parser.add_argument("--mode", choices=("write", "check"), required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    result = render(json.loads(arguments.metadata.read_text()), arguments.source)
    if arguments.mode == "write":
        arguments.output.write_text(result)
    elif arguments.output.read_text() != result:
        raise SystemExit("physical type properties differ from PostgreSQL")


if __name__ == "__main__":
    main()

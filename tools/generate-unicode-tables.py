#!/usr/bin/env python3
"""Convert PostgreSQL's generated Unicode tables into static Rust data."""

import argparse
import pathlib
import re
import subprocess


def array_body(source: str, name: str) -> str:
    declaration = re.search(
        rf"static const [^;{{]+\b{re.escape(name)}\[[^]]+\]\s*=\s*\{{",
        source,
    )
    if declaration is None:
        raise RuntimeError(f"PostgreSQL table {name} was not found")
    opening = declaration.end() - 1
    closing = source.index("\n};", opening)
    return source[opening + 1 : closing]


def chunks(values, size):
    for start in range(0, len(values), size):
        yield values[start : start + size]


def merge_ranges(ranges):
    merged = []
    for first, last in sorted(ranges):
        if merged and first <= merged[-1][1] + 1:
            merged[-1] = (merged[-1][0], max(merged[-1][1], last))
        else:
            merged.append((first, last))
    return merged


def unicode_ranges(source: str, name: str):
    return [
        (int(first, 16), int(last, 16))
        for first, last in re.findall(
            r"\{0x([0-9A-Fa-f]+),\s*0x([0-9A-Fa-f]+)\}",
            array_body(source, name),
        )
    ]


def scalar_array(source: str, name: str):
    body = re.sub(r"/\*.*?\*/", "", array_body(source, name))
    return [int(value, 0) for value in re.findall(r"0x[0-9A-Fa-f]+|\d+", body)]


def case_mapping(source: str, kind: str):
    indexes = [
        (int(codepoint, 16), int(index, 0))
        for index, codepoint in re.findall(
            r"\s*(0x[0-9A-Fa-f]+|\d+),.*?/\* U\+([0-9A-Fa-f]+) \*/",
            array_body(source, "case_map"),
        )
    ]
    simple = scalar_array(source, f"case_map_{kind.lower()}")
    special_indexes = scalar_array(source, "case_map_special")

    special_mappings = [()]
    for values in re.findall(
        rf"\[Case{kind}\]\s*=\s*\{{([^}}]*)\}}",
        array_body(source, "special_case"),
    ):
        special_mappings.append(
            tuple(
                value
                for value in (
                    int(raw, 16)
                    for raw in re.findall(r"0x([0-9A-Fa-f]+)", values)
                )
                if value
            )
        )
    mapping = {}
    for codepoint_value, index in indexes:
        mapped_value = simple[index]
        if mapped_value and mapped_value != codepoint_value:
            mapping[codepoint_value] = (mapped_value,)
        special_index = special_indexes[index]
        # Final sigma is the sole conditional special mapping in PostgreSQL
        # 18. Runtime context selects it; the table retains the ordinary map.
        if special_index and not (kind == "Lower" and codepoint_value == 0x03A3):
            special_mapping = special_mappings[special_index]
            if special_mapping == (codepoint_value,):
                mapping.pop(codepoint_value, None)
            elif special_mapping:
                mapping[codepoint_value] = special_mapping
    return sorted(mapping.items()), special_mappings


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--postgres-source", required=True, type=pathlib.Path)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    args = parser.parse_args()

    include = args.postgres_source / "src/include/common"
    norm = (include / "unicode_norm_table.h").read_text()
    case = (include / "unicode_case_table.h").read_text()
    category = (include / "unicode_category_table.h").read_text()
    commit = subprocess.check_output(
        ["git", "-C", str(args.postgres_source), "rev-parse", "HEAD"], text=True
    ).strip()

    decompositions = []
    for match in re.finditer(
        r"\{0x([0-9A-Fa-f]+),\s*(\d+),\s*([^,]+),\s*(0x[0-9A-Fa-f]+|\d+)\},?",
        array_body(norm, "UnicodeDecompMain"),
    ):
        codepoint, combining_class, flag_text, index = match.groups()
        flags = int(re.match(r"\d+", flag_text).group())
        flags |= 0x80 if "DECOMP_NO_COMPOSE" in flag_text else 0
        flags |= 0x40 if "DECOMP_INLINE" in flag_text else 0
        flags |= 0x20 if "DECOMP_COMPAT" in flag_text else 0
        decompositions.append(
            (int(codepoint, 16), int(combining_class), flags, int(index, 0))
        )

    codepoint_body = re.sub(
        r"/\*.*?\*/", "", array_body(norm, "UnicodeDecomp_codepoints")
    )
    codepoints = [
        int(value, 16)
        for value in re.findall(
            r"0x([0-9A-Fa-f]+)", codepoint_body
        )
    ]

    compositions = []
    for codepoint, _, flags, index in decompositions:
        size = flags & 0x1F
        if size != 2 or flags & (0x80 | 0x20):
            continue
        first, second = codepoints[index : index + 2]
        compositions.append(((first << 32) | second, codepoint))
    compositions.sort()

    lower, special_lower = case_mapping(case, "Lower")
    title, special_title = case_mapping(case, "Title")
    upper, special_upper = case_mapping(case, "Upper")
    folds, special_folds = case_mapping(case, "Fold")

    assigned = []
    for first, last in re.findall(
        r"\{0x([0-9A-Fa-f]+),\s*0x([0-9A-Fa-f]+),\s*PG_U_[A-Z_]+\}",
        array_body(category, "unicode_categories"),
    ):
        pair = (int(first, 16), int(last, 16))
        if assigned and assigned[-1][1] + 1 == pair[0]:
            assigned[-1] = (assigned[-1][0], pair[1])
        else:
            assigned.append(pair)

    case_ignorable = unicode_ranges(category, "unicode_case_ignorable")
    cased = merge_ranges(
        unicode_ranges(category, "unicode_lowercase")
        + unicode_ranges(category, "unicode_uppercase")
        + [
            (int(first, 16), int(last, 16))
            for first, last in re.findall(
                r"\{0x([0-9A-Fa-f]+),\s*0x([0-9A-Fa-f]+),\s*PG_U_TITLECASE_LETTER\}",
                array_body(category, "unicode_categories"),
            )
        ]
    )
    alphanumeric = merge_ranges(
        unicode_ranges(category, "unicode_alphabetic")
        + [
            (int(first, 16), int(last, 16))
            for first, last in re.findall(
                r"\{0x([0-9A-Fa-f]+),\s*0x([0-9A-Fa-f]+),\s*PG_U_DECIMAL_NUMBER\}",
                array_body(category, "unicode_categories"),
            )
        ]
    )

    if len(decompositions) != 6843 or len(codepoints) != 5138:
        raise RuntimeError("unexpected PostgreSQL 18 normalization table shape")
    if not all(
        len(special) == 106
        for special in (special_lower, special_title, special_upper, special_folds)
    ):
        raise RuntimeError("unexpected PostgreSQL 18 special-case table shape")

    lines = [
        "//! @generated by tools/generate-unicode-tables.py; do not edit.",
        "//! Unicode 16 data generated from PostgreSQL 18.",
        "//!",
        f"//! Source commit: https://github.com/postgres/postgres/commit/{commit}",
        "//! Inputs: `src/include/common/unicode_{norm,case,category}_table.h`.",
        "//! Regenerate with `tools/generate-unicode-tables.py`.",
        "",
        "#[derive(Clone, Copy)]",
        "pub(crate) struct Decomposition {",
        "    pub(crate) codepoint: u32,",
        "    pub(crate) combining_class: u8,",
        "    pub(crate) flags: u8,",
        "    pub(crate) index: u16,",
        "}",
        "",
        f"pub(crate) static DECOMPOSITIONS: [Decomposition; {len(decompositions)}] = [",
    ]
    lines.extend(
        f"    Decomposition {{ codepoint: 0x{cp:X}, combining_class: {comb}, flags: 0x{flags:02X}, index: {index} }},"
        for cp, comb, flags, index in decompositions
    )
    lines.extend(
        [
            "];",
            "",
            f"pub(crate) static DECOMPOSITION_CODEPOINTS: [u32; {len(codepoints)}] = [",
        ]
    )
    for group in chunks(codepoints, 8):
        lines.append("    " + ", ".join(f"0x{value:X}" for value in group) + ",")
    lines.extend(
        [
            "];",
            "",
            f"pub(crate) static COMPOSITIONS: [(u64, u32); {len(compositions)}] = [",
        ]
    )
    lines.extend(f"    (0x{key:016X}, 0x{value:X})," for key, value in compositions)
    lines.extend(
        [
            "];",
            "",
            "",
        ]
    )
    for name, mappings in (
        ("CASE_LOWER", lower),
        ("CASE_TITLE", title),
        ("CASE_UPPER", upper),
        ("CASE_FOLDS", folds),
    ):
        lines.append(
            f"pub(crate) static {name}: [(u32, [u32; 3], u8); {len(mappings)}] = ["
        )
        for source, mapping in mappings:
            padded = mapping + (0,) * (3 - len(mapping))
            lines.append(
                f"    (0x{source:X}, [0x{padded[0]:X}, 0x{padded[1]:X}, 0x{padded[2]:X}], {len(mapping)}),"
            )
        lines.extend(["];", ""])
    lines.extend(
        [
            f"pub(crate) static ASSIGNED_RANGES: [(u32, u32); {len(assigned)}] = [",
        ]
    )
    lines.extend(f"    (0x{first:X}, 0x{last:X})," for first, last in assigned)
    lines.extend(
        [
            "];",
            "",
            f"pub(crate) static CASED_RANGES: [(u32, u32); {len(cased)}] = [",
        ]
    )
    lines.extend(f"    (0x{first:X}, 0x{last:X})," for first, last in cased)
    lines.extend(
        [
            "];",
            "",
            f"pub(crate) static ALPHANUMERIC_RANGES: [(u32, u32); {len(alphanumeric)}] = [",
        ]
    )
    lines.extend(f"    (0x{first:X}, 0x{last:X})," for first, last in alphanumeric)
    lines.extend(
        [
            "];",
            "",
            f"pub(crate) static CASE_IGNORABLE_RANGES: [(u32, u32); {len(case_ignorable)}] = [",
        ]
    )
    lines.extend(f"    (0x{first:X}, 0x{last:X})," for first, last in case_ignorable)
    lines.append("];")
    lines.append("")
    args.output.write_text("\n".join(lines))
    subprocess.check_call(["rustfmt", str(args.output)])


if __name__ == "__main__":
    main()

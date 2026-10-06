#!/usr/bin/env python3
"""Check local Markdown destinations and heading fragments without network I/O."""

import argparse
from pathlib import Path
import re
import subprocess
import sys
from urllib.parse import unquote, urlsplit


def prose(text):
    fenced = False
    marker = ""
    for number, line in enumerate(text.splitlines(), 1):
        stripped = line.lstrip()
        if stripped.startswith(("```", "~~~")):
            candidate = stripped[:3]
            if not fenced:
                fenced, marker = True, candidate
            elif candidate == marker:
                fenced = False
            continue
        if not fenced:
            yield number, line


def fragments(text):
    result = set()
    counts = {}
    for _, line in prose(text):
        heading = re.match(r"^ {0,3}#{1,6}\s+(.+?)\s*#*\s*$", line)
        if not heading:
            continue
        slug = re.sub(r"[^\w\- ]", "", heading[1].lower()).replace(" ", "-")
        count = counts.get(slug, 0)
        counts[slug] = count + 1
        result.add(slug if count == 0 else f"{slug}-{count}")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, help="Check an extracted documentation tree")
    arguments = parser.parse_args()
    if arguments.root is None:
        root = Path(__file__).resolve().parent.parent
        names = subprocess.check_output(
            ["git", "ls-files", "-z", "--", "*.md"], cwd=root
        ).decode().split("\0")
        files = [root / name for name in names if name]
    else:
        root = arguments.root.resolve()
        files = sorted(root.rglob("*.md"))
    errors = []
    checked = 0
    headings = {}
    for source in files:
        for number, line in prose(source.read_text()):
            for match in re.finditer(r"\[[^\n]*?\]\(([^)\n]+)\)", line):
                destination = match[1].strip().split(' "', 1)[0].strip("<>")
                url = urlsplit(destination)
                if url.scheme or url.netloc:
                    continue
                checked += 1
                target = (source.parent / unquote(url.path)).resolve() if url.path else source
                location = f"{source.relative_to(root)}:{number}"
                if not target.exists():
                    errors.append(f"{location}: missing destination {destination}")
                elif url.fragment and target.suffix == ".md":
                    if target not in headings:
                        headings[target] = fragments(target.read_text())
                    if unquote(url.fragment) not in headings[target]:
                        errors.append(f"{location}: missing heading {destination}")
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    if not files:
        print("documentation guard: no Markdown files found", file=sys.stderr)
        return 1
    print(f"documentation guard: {len(files)} files, {checked} local links (OK)")
    return 0


if __name__ == "__main__":
    sys.exit(main())

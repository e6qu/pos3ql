#!/usr/bin/env bash
# Library test suite guard, checked from source: the `unit-suite` and
# `sql-fixture-suite` builds must split the tests between them (CI's
# check-lib-test-suites.py checks the built lists). An inline module holding
# `#[test]` functions under a bare `#[cfg(test)]` lands in both suites, so
# each such module outside `sql::tests` must exclude the fixture suite, or
# gate each of its tests on its own.
#
# Usage: tools/check-test-suites.sh   (exit 0 = clean, 1 = a module in both)

set -u
cd "$(dirname "$0")/.."

python3 - <<'PY'
import pathlib, re, sys

bare = re.compile(r'^#\[cfg\(test\)\]\s*\n(?:#\[[^\n]*\]\s*\n)*(?:pub(?:\([^)]*\))? )?mod (\w+) \{', re.M)
violations = 0
for path in sorted(pathlib.Path('src').rglob('*.rs')):
    if path.parts[:3] == ('src', 'sql', 'tests'):
        continue
    text = path.read_text(encoding='utf-8')
    for match in bare.finditer(text):
        depth, at = 0, match.end() - 1
        for at in range(match.end() - 1, len(text)):
            if text[at] == '{':
                depth += 1
            elif text[at] == '}':
                depth -= 1
                if depth == 0:
                    break
        body = text[match.end():at].split('\n')
        ungated = any(
            line.strip() == '#[test]'
            and not any('sql-fixture-suite' in prior for prior in body[max(0, i - 3):i]
                        if prior.strip().startswith('#['))
            for i, line in enumerate(body))
        if ungated:
            line = text.count('\n', 0, match.start()) + 1
            print(f'  BOTH SUITES  {path}:{line}: mod {match.group(1)} needs '
                  '#[cfg(all(test, not(feature = "sql-fixture-suite")))]')
            violations += 1
print(f'\ntest suite guard: {violations} violations')
sys.exit(1 if violations else 0)
PY

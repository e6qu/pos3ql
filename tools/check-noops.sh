#!/usr/bin/env bash
# No-op guard: fails if the source silently accepts-and-ignores SQL/protocol
# semantics. A "no-op" here means code that reports success while skipping
# behavior a client can observe — the class of bug that lets us quietly skip
# implementing something we ought to. Idempotent operations ("no-op when
# nothing changed") are a different thing and are not matched.
#
# Every match must be removed: implement the behavior or reject it loudly.
# Fixable work cannot be exempted with a debt marker or deferred in BUGS.md.
#
# Usage: tools/check-noops.sh   (exit 0 = clean, 1 = a semantic no-op)

set -u
cd "$(dirname "$0")/.."

# Phrases that mark a silent semantic no-op. Precise on purpose: these are the
# ways "we pretend to handle X but don't" get written, not the word "no-op"
# (which legitimately describes idempotency).
BANNED='accepted and ignored|accepted and has no effect|accepted.*no effect|for client compatibility|parsed and discarded|value is skipped|accepted as a no-?op|silently (ignore|ignored|skip|skipped|default|drop|dropped)'

violations=0
while IFS= read -r hit; do
  [[ -z "$hit" ]] && continue
  violations=$((violations + 1))
  printf '  NO-OP  %s\n' "$hit"
done < <(grep -rniE "$BANNED" src --include='*.rs')

printf '\nno-op guard: %s violations\n' "$violations"

if (( violations > 0 )); then
  printf '%s\n' 'FAIL: implement the behavior or reject it loudly. Debt markers do not'
  printf '%s\n' 'exempt fixable work, and BUGS.md is not a deferral backlog.'
  exit 1
fi
printf '%s\n' OK
exit 0

#!/bin/zsh
# No-op guard: fails if the source silently accepts-and-ignores SQL/protocol
# semantics. A "no-op" here means code that reports success while skipping
# behavior a client can observe — the class of bug that lets us quietly skip
# implementing something we ought to. Idempotent operations ("no-op when
# nothing changed") are a different thing and are not matched.
#
# Every match must either be removed (implement it, or reject it loudly) or,
# during a burn-down, be tagged on the same line with a bare NOOP-DEBT marker.
# The total tagged debt may not exceed DEBT_BUDGET, so the count can only
# ratchet down. (The marker carries no bug id — those rot in source; the
# burn-down items live in BUGS.md, cited from commits, not code.)
#
# Usage: tools/check-noops.sh   (exit 0 = clean, 1 = a new/untracked no-op)

set -u
cd "$(dirname "$0")/.."

# Phrases that mark a silent semantic no-op. Precise on purpose: these are the
# ways "we pretend to handle X but don't" get written, not the word "no-op"
# (which legitimately describes idempotency).
BANNED='accepted and ignored|for client compatibility|parsed and discarded|value is skipped|accepted as a no-?op|silently (ignore|ignored|skip|skipped|default|drop|dropped)'

# The tagged-debt ceiling. Lower it as items are implemented; never raise it.
DEBT_BUDGET=0

violations=0
debt=0
while IFS= read -r hit; do
  [[ -z "$hit" ]] && continue
  if [[ "$hit" == *'NOOP-DEBT'* ]]; then
    debt=$((debt + 1))
    print -- "  debt   $hit"
  else
    violations=$((violations + 1))
    print -- "  NEW    $hit"
  fi
done < <(grep -rniE "$BANNED" src --include='*.rs')

print -- ""
print -- "no-op guard: $violations untracked, $debt tracked debt (budget $DEBT_BUDGET)"

if (( violations > 0 )); then
  print -- "FAIL: untracked no-op(s). Implement it, reject it loudly, or (during"
  print -- "burn-down) tag the line with a NOOP-DEBT marker and log it in BUGS.md."
  exit 1
fi
if (( debt > DEBT_BUDGET )); then
  print -- "FAIL: tracked no-op debt $debt exceeds budget $DEBT_BUDGET (ratchet only down)."
  exit 1
fi
print -- "OK"
exit 0

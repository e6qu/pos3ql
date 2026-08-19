#!/usr/bin/env bash
# BUGS.md discipline guard: BUGS.md is not a backlog. It is reserved for bugs
# that genuinely cannot be fixed right now, and every `open` row must say *why*
# it is intractable. This guard fails if an `open` row either (a) carries no
# intractability justification, or (b) reads as a deferral of fixable work
# ("its own PR", "follow-up", "fresh session", …). The failure mode it stops:
# parking a fixable bug as `open` instead of fixing it. It is the BUGS.md
# analogue of tools/check-noops.sh.
#
# Usage: tools/check-bugs.sh   (exit 0 = clean, 1 = an unjustified open bug)

set -u
cd "$(dirname "$0")/.."

# An `open` row must contain at least one of these — a stated reason the bug
# cannot be fixed now (environmental non-reproducibility, or a real-PostgreSQL
# internal that can't be matched without imitating their planner/hash/qsort).
JUSTIFY='intractable|by[- ]design|unmatchable|environmental|planner[- ]internal|implementation detail|not a fixed rule|not any specified behavior|needs a dedicated|hash order|qsort|reproduc'

# An `open` row must contain NONE of these — they are how deferral of fixable
# work gets written. Finding one means: do the fix, do not park the bug.
DEFERRAL='its own PR|own PR|follow-?up PR|fresh session|out of scope|deserving its own|pick(ed)? (this )?up|a later PR|next PR|separate PR|for now|when we get to'

violations=0
open_rows=0
while IFS= read -r line; do
  # Only table rows for a bug id.
  [[ "$line" == \|\ B-* ]] || continue
  row_status="${line#*|}"          # drop leading "| "
  row_status="${row_status#*|}"    # now starts at the status cell + rest
  row_status="${row_status%%|*}"   # take up to next pipe = status cell
  row_status="${row_status// /}"   # trim spaces
  [[ "$row_status" == "open" ]] || continue
  open_rows=$((open_rows + 1))
  id="${line#*B-}"; id="B-${id%% *}"; id="${id%%|*}"; id="${id// /}"

  if printf '%s\n' "$line" | grep -qiE "$DEFERRAL"; then
    violations=$((violations + 1))
    printf "%s\n" "  DEFERRAL  $id — open row reads as a deferral of fixable work; fix it, don't park it"
    continue
  fi
  if ! printf '%s\n' "$line" | grep -qiE "$JUSTIFY"; then
    violations=$((violations + 1))
    printf "%s\n" "  UNJUSTIFIED  $id — open row has no stated reason it can't be fixed now"
  fi
done < BUGS.md

printf '\nBUGS.md guard: %s open, %s unjustified/deferred\n' "$open_rows" "$violations"

if (( violations > 0 )); then
  printf '%s\n' 'FAIL: an open bug is either unjustified or a deferral. BUGS.md is for'
  printf '%s\n' 'genuinely-intractable bugs only — fix it in this PR, or state loudly why'
  printf '%s\n' 'it cannot be fixed now (see AGENTS.md, the Boyscout Rule).'
  exit 1
fi
printf '%s\n' OK
exit 0

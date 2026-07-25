# AGENTS.md

Instructions for any agent (human or AI) working in this repository.

**Project documents** (all cross-linked): [README.md](README.md) — architecture
and quick start · [PLAN.md](PLAN.md) — roadmap · [BUGS.md](BUGS.md) — known bugs
· [docs/terminology.md](docs/terminology.md) — glossary and naming rules.

## The Boyscout Rule — HARD RULE, ALWAYS IN EFFECT

**Leave every file better than you found it. If you encounter something broken — anything — you fix it, even if it looks unrelated to the task at hand.**

- **Everything is related.** It is one system, one codebase, one shared effort. When a
  bug looks "unrelated," that is an artifact of a limited context window (yours or a
  future agent's), not a fact about the code. "Unrelated" is never a reason to walk past
  something broken.
- **Do not file-and-forget.** A BUGS.md entry is not a substitute for a fix. Track a bug
  only when it is genuinely intractable right now (and say *why* it is intractable, not
  merely "narrow" or "out of scope"). **BUGS.md is not a backlog or a to-do list** — it is
  reserved for bugs that cannot be fixed now, each carrying its explicit reason. Filing a
  fixable bug as `open` instead of fixing it is the failure mode this rule exists to stop,
  and it is enforced mechanically: `tools/check-bugs.sh` (run with `zsh`, wired into CI)
  fails if any `open` row lacks an intractability justification or reads as a deferral
  ("its own PR", "follow-up", "fresh session", …). If your instinct is to write a new
  `B-NNN`, that is the signal to write the fix instead.
- **Do not triage by relevance.** "Narrow," "pre-existing," "not the primary ask," and
  "tangent" are banned as reasons to skip a fix. The only legitimate reasons to defer are
  genuine intractability or a true blocker — and those get stated loudly.
- **The bug the codebase hands you is the next task.** If you trip over a second bug while
  fixing the first, you now have two fixes to make, not one fix and one note.
- Difficulty is a real constraint; unworthiness is not. If a fix is genuinely large, say so
  explicitly and either do it or flag it as a real blocker — never quietly downgrade it.

## Other standing directives

These reinforce, and are subordinate to, the Boyscout Rule.

- **Full PostgreSQL fidelity; never paper over gaps.** Implement wire + SQL fully; compare
  strictly against real PostgreSQL. Vanilla PG only. Ask before deferring genuinely exotic
  features.
- **No silent fallbacks.** No empty catches, no "log and continue" in logic, no optional
  defaults that hide failure. (Network retries/backoff are different and are fine.)
- **No silent no-ops.** Accept-and-ignore of client-observable semantics is banned;
  enforced by `tools/check-noops.sh` (run with `zsh`) + `cargo test` + CI.
- **Fix bug *classes* structurally, not punctually.** Prefer newtypes, choke points, and
  states-as-types so a whole class of bugs becomes impossible. Name the class in the commit.
- **If you spend a paragraph justifying a line of code, the code is wrong — fix the code.**
  Long defensive comments are refactor signals, not documentation.
- **Provenance required.** Never state facts unsourced; downloaded artifacts need
  pinned-commit provenance metadata.
- **Static-memory discipline.** No heap allocation after startup; allocation-free sorts;
  exhausting any pool is a loud error, never growth. Runtime dependencies are `libc` and
  the isolated `rustls` TLS component (the one whitelisted exception — every call runs
  inside `mem::guard::tls_scope`, charged against `tls_pool_bytes`, aborting loudly past
  it; see PLAN.md Stage G).
- **Docs:** update `PLAN.md` and `BUGS.md` in the same PR as the work. No phase numbers or
  BUGS IDs in source or code comments — the "why" belongs in commit messages.
- **Names are spelled out.** No non-obvious abbreviations: `interval` not `iv`, `buffer`
  not `buf`, `expression` not `expr`, `statement` not `stmt`, `index` not `idx`. Well-known
  acronyms (`AWS`, `ECS`, `S3`, `SQL`, `JSON`, `UUID`, `HTTP`, …) are fine. Limit coined
  project terminology; when a term is unavoidable, define it in
  [`docs/terminology.md`](docs/terminology.md) — the canonical glossary and naming rules,
  cross-linked from every doc.
- **Commits:** PRs are squash-merged, so one commit per PR is fine; don't make fine-grained
  commits inside a PR.
- **Batch big; no micro-PRs, no deferrals.** Prefer one large, complete PR over a chain of
  small ones. Do all the related work — the feature, its Boyscout fixes, the incidental bugs
  you trip over, and the docs (`PLAN.md`, `BUGS.md`, `AGENTS.md`, `README.md`,
  `docs/`) — in the *same* PR. "Its own follow-up PR", "scoped to a corpus", and "a fresh
  session can pick this up" are deferrals; per the Boyscout Rule they are banned unless the
  work is genuinely intractable or a true blocker, stated loudly. Finish the whole chunk.

# AGENTS.md

Project documents: [README.md](README.md), [PLAN.md](PLAN.md), [BUGS.md](BUGS.md), and [docs/terminology.md](docs/terminology.md).

## Boyscout Rule

Leave every file better than you found it. Fix every bug you encounter, including incidental ones. Do not defer fixable work by filing it in BUGS.md, splitting it into a follow-up, or calling it unrelated. Record a bug only when an external blocker or genuine intractability prevents a fix, and state that blocker. `tools/check-bugs.sh` enforces this rule.

## Engineering rules

- Match vanilla PostgreSQL strictly at SQL and wire boundaries. Reject unsupported client-visible behavior loudly; never accept and ignore it.
- Do not use silent fallbacks. Retries and backoff are not fallbacks.
- Fix bug classes structurally: use types, parse boundaries, and choke points that make invalid states unrepresentable.
- Runtime memory is fixed at startup. No post-startup heap allocation, growing pools, or allocating sorts. Exhaustion is a named error. Runtime dependencies are `libc` and the isolated, budgeted rustls object-store TLS component.
- Object storage is the durable tier. RAM and disk are caches. Do not add provider-specific behavior above the provider-neutral storage contract.
- Spell names out as defined in [docs/terminology.md](docs/terminology.md).
- Keep comments and docs concise. Explain non-obvious invariants and externally visible behavior, not a line-by-line restatement of code.

## Change rules

- Update PLAN.md and BUGS.md in every PR.
- Do not put phase numbers or BUG IDs in source comments.
- Preserve provenance for stated facts and downloaded artifacts.
- PRs are squash-merged; one commit is sufficient.
- Batch related implementation, tests, fixes, and documentation into one complete PR. Do not create micro-PRs or defer connected work without a real blocker.

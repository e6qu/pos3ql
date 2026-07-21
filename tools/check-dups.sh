#!/bin/sh
# Copy-paste guard: runs jscpd (v5, a Rust-tokenizing Rabin-Karp copy-paste
# detector) against the committed .jscpd.json. Fails if duplication exceeds the
# ratchet threshold there. Mirrors tools/check-noops.sh; requires node/npx.
here=$(dirname "$0")
exec npx --yes jscpd@5 --config "$here/../.jscpd.json"

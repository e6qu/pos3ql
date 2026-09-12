#!/bin/sh
# Copy-paste guard: a local normalized-line detector with no network or runtime
# package dependency. Its threshold is a ratchet over committed Rust source.
here=$(dirname "$0")
exec python3 "$here/check_dups.py"
